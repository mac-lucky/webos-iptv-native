//! The channel name editor's rules: prefixes and suffixes providers put on channel names (a
//! country code, a quality tag) that the lists should not show.
//!
//! The rules live in the settings, one per line ([`keys::NAMES_PREFIXES`],
//! [`keys::NAMES_SUFFIXES`]). [`NameRules::clean`] is applied once, where the data layer hands
//! channels to the screens, so every list, the player and the menus show the same name; the
//! guide matching and the stored playlist keep the provider's name.
//!
//! A rule matches without regard to ASCII case and only on a word boundary: `PL` strips
//! `PL: TVN` and `PL TVN` but leaves `PLANETE+`, and `HD` strips `TVN HD` but leaves
//! `Polsat FHD`. The separators and spaces left at the cut go with it. A name is never cleaned
//! down to nothing.

use crate::panels::keys;
use crate::panels::settings::Settings;

/// The longest rule: a tag, not a sentence.
pub const RULE_MAX_CHARS: usize = 32;
/// The most rules on each list.
pub const RULES_MAX: usize = 50;

/// Which end of a name a rule strips.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum End {
    Prefix,
    Suffix,
}

impl End {
    /// The setting that holds this end's rules.
    pub fn key(self) -> &'static str {
        match self {
            End::Prefix => keys::NAMES_PREFIXES,
            End::Suffix => keys::NAMES_SUFFIXES,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameRules {
    prefixes: Vec<String>,
    suffixes: Vec<String>,
}

impl NameRules {
    pub fn from_settings(settings: &Settings) -> Self {
        NameRules {
            prefixes: rules(settings, End::Prefix).map(str::to_owned).collect(),
            suffixes: rules(settings, End::Suffix).map(str::to_owned).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty() && self.suffixes.is_empty()
    }

    /// `name` without the configured prefixes and suffixes, as often as they match.
    pub fn clean(&self, name: &str) -> String {
        let mut s = name.trim();
        if self.is_empty() {
            return s.to_owned();
        }
        // A round strips what the one before uncovered ("VIP: PL: TVN"); usually one or two run,
        // and the rule count bounds them whatever a name repeats.
        for _ in 0..=self.prefixes.len() + self.suffixes.len() {
            let before = s;
            for rule in &self.prefixes {
                if let Some(rest) = strip_prefix(s, rule) {
                    s = rest;
                }
            }
            for rule in &self.suffixes {
                if let Some(rest) = strip_suffix(s, rule) {
                    s = rest;
                }
            }
            if s == before {
                break;
            }
        }
        s.to_owned()
    }
}

/// The rules of `end`, as stored.
pub fn rules(settings: &Settings, end: End) -> impl Iterator<Item = &str> {
    settings
        .get(end.key())
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
}

/// Why a typed rule was not added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Empty, too long or with characters a rule cannot hold.
    Invalid,
    /// The list has it already (in any ASCII case).
    Exists,
    Full,
}

/// The value of `end`'s setting with `rule` (trimmed) added.
pub fn with_rule(settings: &Settings, end: End, rule: &str) -> Result<String, Refused> {
    let rule = rule.trim();
    if rule.is_empty()
        || rule.chars().count() > RULE_MAX_CHARS
        || rule.chars().any(char::is_control)
    {
        return Err(Refused::Invalid);
    }
    let mut list: Vec<&str> = rules(settings, end).collect();
    if list.iter().any(|r| r.eq_ignore_ascii_case(rule)) {
        return Err(Refused::Exists);
    }
    if list.len() >= RULES_MAX {
        return Err(Refused::Full);
    }
    list.push(rule);
    Ok(list.join("\n"))
}

/// The value of `end`'s setting without its `index`th rule.
pub fn without_rule(settings: &Settings, end: End, index: usize) -> String {
    rules(settings, end)
        .enumerate()
        .filter(|&(i, _)| i != index)
        .map(|(_, r)| r)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What is left at a cut: spaces and the separators providers put between a tag and a name.
fn separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ':' | '|' | '-' | '/' | '_' | '.' | ',')
}

fn word(c: char) -> bool {
    c.is_alphanumeric()
}

/// `s` without `rule` at its start, when it is there on a word boundary and leaves a name.
fn strip_prefix<'a>(s: &'a str, rule: &str) -> Option<&'a str> {
    let head = s.get(..rule.len())?;
    let rest = &s[rule.len()..];
    if !head.eq_ignore_ascii_case(rule) {
        return None;
    }
    let joined = rule.chars().last().is_some_and(word) && rest.chars().next().is_some_and(word);
    let rest = rest.trim_start_matches(separator);
    (!joined && !rest.is_empty()).then_some(rest)
}

/// `s` without `rule` at its end, when it is there on a word boundary and leaves a name.
fn strip_suffix<'a>(s: &'a str, rule: &str) -> Option<&'a str> {
    let cut = s.len().checked_sub(rule.len())?;
    let tail = s.get(cut..)?;
    let rest = &s[..cut];
    if !tail.eq_ignore_ascii_case(rule) {
        return None;
    }
    let joined = rule.chars().next().is_some_and(word) && rest.chars().last().is_some_and(word);
    let rest = rest.trim_end_matches(separator);
    (!joined && !rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(prefixes: &str, suffixes: &str) -> NameRules {
        NameRules::from_settings(&Settings::from_stored([
            (keys::NAMES_PREFIXES, prefixes),
            (keys::NAMES_SUFFIXES, suffixes),
        ]))
    }

    #[test]
    fn country_codes_and_quality_tags_go() {
        let r = rules("PL:\nUK |\nVIP", "HD\nFHD\n4K\n(backup)");
        for (raw, shown) in [
            ("PL: TVN HD", "TVN"),
            ("PL:TVN", "TVN"),
            ("pl: Polsat FHD", "Polsat"),
            ("UK | BBC One 4K", "BBC One"),
            ("VIP: PL: Canal+ Sport HD", "Canal+ Sport"),
            ("TVP Info (backup)", "TVP Info"),
            ("  Eurosport 1 HD  ", "Eurosport 1"),
            ("Kino Polska", "Kino Polska"),
        ] {
            assert_eq!(r.clean(raw), shown, "{raw}");
        }
    }

    #[test]
    fn rules_match_on_word_boundaries_only() {
        let r = rules("PL", "HD");
        assert_eq!(r.clean("PL: TVN"), "TVN");
        assert_eq!(r.clean("PL TVN"), "TVN");
        assert_eq!(r.clean("PL|TVN"), "TVN");
        assert_eq!(r.clean("PLANETE+"), "PLANETE+", "inside a word");
        assert_eq!(r.clean("Polsat FHD"), "Polsat FHD", "FHD is not HD");
        assert_eq!(r.clean("TVN HD+"), "TVN HD+", "not at the end");
        assert_eq!(r.clean("TVN-HD"), "TVN");
    }

    #[test]
    fn a_name_is_never_cleaned_away() {
        let r = rules("PL:", "HD");
        assert_eq!(r.clean("PL:"), "PL:");
        assert_eq!(r.clean("HD"), "HD");
        assert_eq!(
            r.clean("PL: HD"),
            "HD",
            "the prefix goes, the last word stays"
        );
        assert_eq!(rules("", "").clean(" TVN "), "TVN");
    }

    #[test]
    fn non_ascii_names_and_rules_cut_on_character_boundaries() {
        let r = rules("\u{141}\u{f3}d\u{17a}:", "\u{17c}");
        assert_eq!(r.clean("\u{141}\u{f3}d\u{17a}: TVN"), "TVN");
        assert_eq!(r.clean("Kana\u{142} \u{17c}"), "Kana\u{142}");
        // A rule longer than the name, or ending inside a character, matches nothing.
        assert_eq!(rules("TVN Sport", "").clean("TVN"), "TVN");
        assert_eq!(rules("e", "").clean("\u{e9} x"), "\u{e9} x");
        assert_eq!(rules("", "x").clean("a \u{e9}"), "a \u{e9}");
    }

    #[test]
    fn rules_are_added_trimmed_once_and_removed_by_index() {
        let s = Settings::new();
        let one = with_rule(&s, End::Prefix, "  PL: ").unwrap();
        assert_eq!(one, "PL:");
        let s = Settings::from_stored([(keys::NAMES_PREFIXES, one.as_str())]);
        assert_eq!(with_rule(&s, End::Prefix, "pl:"), Err(Refused::Exists));
        assert_eq!(with_rule(&s, End::Prefix, "   "), Err(Refused::Invalid));
        assert_eq!(
            with_rule(&s, End::Prefix, &"x".repeat(RULE_MAX_CHARS + 1)),
            Err(Refused::Invalid)
        );
        let two = with_rule(&s, End::Prefix, "UK:").unwrap();
        assert_eq!(two, "PL:\nUK:");
        let s = Settings::from_stored([(keys::NAMES_PREFIXES, two.as_str())]);
        assert_eq!(without_rule(&s, End::Prefix, 0), "UK:");
        assert_eq!(without_rule(&s, End::Prefix, 5), "PL:\nUK:");
        let full: Vec<String> = (0..RULES_MAX).map(|i| format!("R{i}")).collect();
        let s = Settings::from_stored([(keys::NAMES_SUFFIXES, full.join("\n"))]);
        assert_eq!(with_rule(&s, End::Suffix, "HD"), Err(Refused::Full));
    }
}
