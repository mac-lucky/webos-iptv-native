use super::*;
#[test]
fn the_webos_release_line_reads_unknown_rather_than_inventing_a_release() {
    let known = crate::tv::device::Info { release: "4.10.2".into(), major: 4, ..Default::default() };
    assert_eq!(webos_release_line(&known), "webOS 4.10.2");
    assert_eq!(webos_release_line(&crate::tv::device::Info::default()), msg::browse_diagnostics_unknown_os());
}
#[test]
fn locale_resolution_separates_language_from_formatting() {
    let regional = LocaleContext::resolve(Preference::System, Some("en-GB"), None, None, None);
    assert_eq!(regional.format_locale(), "en-GB");
    assert_eq!(regional.date(2026, 9, 27).as_deref(), Some("27/09/2026"));
    let cx = LocaleContext::resolve(
        Preference::Pl,
        Some("en-GB"),
        Some("de-DE"),
        Some("24"),
        None,
    );
    assert_eq!(cx.format_locale(), "de-DE");
    assert_eq!(cx.language(), Language::Pl);
    assert_eq!(cx.preference(), Preference::Pl);
    assert_eq!(cx.number(12345), "12.345");
    assert_eq!(cx.decimal(45, 1), "4,5");
    assert_eq!(cx.clock(), Clock::H24);
    assert_eq!(cx.date(2026, 9, 27).as_deref(), Some("27.09.2026"));
    assert_eq!(
        LocaleContext::resolve(Preference::System, None, None, None, Some("pl_PL.UTF-8"))
            .language(),
        Language::Pl
    );
    for tag in ["pl-PL", "pl_PL", "pl"] {
        let cx = LocaleContext::resolve(Preference::System, Some(tag), None, None, None);
        assert_eq!(cx.language(), Language::Pl);
        assert_eq!(cx.preference(), Preference::System);
        assert_eq!(cx.format_locale(), "pl-PL");
    }
    let pl = LocaleContext::resolve(Preference::Pl, None, None, None, None);
    assert_eq!(pl.format_locale(), "pl-PL");
    assert_eq!(pl.date(2026, 9, 27).as_deref(), Some("27.09.2026"));
    assert_eq!(pl.decimal(45, 1), "4,5");
    for tag in ["pt-BR", "de-AT", "pl-Cyrl", "garbage\r\nheader", "C"] {
        assert_eq!(
            LocaleContext::resolve(Preference::System, Some(tag), None, None, None).language(),
            Language::En
        );
    }
}
#[test]
fn plural_rules_follow_the_message_language() {
    // Formatting in English must not change the Polish message's plural rules.
    let pl = LocaleContext::resolve(Preference::Pl, None, Some("en-US"), None, None);
    assert_eq!(pl.plural(1), PluralCategory::One);
    for n in [2, 3, 4, 22, 24, 103] {
        assert_eq!(pl.plural(n), PluralCategory::Few);
    }
    for n in [0, 5, 11, 12, 14, 21, 25, 111, 112] {
        assert_eq!(pl.plural(n), PluralCategory::Many);
    }
    let mut fraction = Decimal::from(15);
    fraction.multiply_pow10(-1);
    assert_eq!(pl.plural.category_for(&fraction), PluralCategory::Other);
    let en = LocaleContext::resolve(Preference::En, None, Some("pl-PL"), None, None);
    assert_eq!(en.plural(1), PluralCategory::One);
    for n in [0, 2, 21, 1_000_000] {
        assert_eq!(en.plural(n), PluralCategory::Other);
    }
}

#[test]
fn polish_count_messages_render_the_reviewed_forms() {
    let locale = LocaleContext::resolve(Preference::Pl, None, None, None, None);
    for (count, expected) in [
        (1, "1 element"),
        (2, "2 elementy"),
        (5, "5 element\u{f3}w"),
        (12, "12 element\u{f3}w"),
        (22, "22 elementy"),
        (25, "25 element\u{f3}w"),
    ] {
        assert_eq!(msg::browse_search_items_in(&locale, count), expected);
    }
    for (count, expected) in [(1, "1 dzie\u{144}"), (2, "2 dni"), (5, "5 dni"), (14, "14 dni")] {
        assert_eq!(msg::unit_days_in(&locale, count), expected);
    }
    let en = LocaleContext::resolve(Preference::En, None, None, None, None);
    assert_eq!(msg::unit_days_in(&en, 1), "1 day");
    assert_eq!(msg::unit_days_in(&en, 7), "7 days");
}
#[test]
fn settings_replies_are_typed_and_refusals_do_not_look_like_success() {
    let s=parse_reply(r#"{"returnValue":true,"settings":{"localeInfo":{"locales":{"UI":"pl_PL","FMT":"en-GB"},"clock":"12"}}}"#).unwrap();
    assert_eq!(s.ui.as_deref(), Some("pl-PL"));
    assert_eq!(s.fmt.as_deref(), Some("en-GB"));
    assert_eq!(s.clock.as_deref(), Some("12"));
    for s in [
        "",
        "not json",
        r#"{"returnValue":false}"#,
        r#"{"returnValue":true,"settings":{}}"#,
    ] {
        assert!(parse_reply(s).is_none());
    }
    assert!(normalize("es-ES\r\nX-Test: injected").is_none());
    assert!(normalize("POSIX").is_none());
}
#[test]
fn all_catalog_glyphs_exist_in_both_shipped_text_faces() {
    for name in ["appfont.ttf", "appfont-bold.ttf"] {
        let cov = tv_base::fontcov::shipped(name).as_ref().unwrap();
        for s in msg::CORPUS.iter().copied().chain(["\u{105}\u{107}\u{119}\u{142}\u{144}\u{f3}\u{15b}\u{17a}\u{17c}\u{104}\u{106}\u{118}\u{141}\u{143}\u{d3}\u{15a}\u{179}\u{17b}\u{201e}\u{201d}"]) {
            for ch in s.chars().filter(|c| !c.is_control()) {
                assert!(
                    cov.contains(ch as u32),
                    "{name}: missing {ch:?} U+{:04X}",
                    ch as u32
                );
            }
        }
    }
}

#[test]
fn pseudo_locale_exercises_expansion_without_changing_arguments() {
    let mut cx = LocaleContext::resolve(Preference::En, None, None, None, None);
    cx.language = Language::Pseudo;
    let text = msg::core_shared_by_in(&cx, "name {literal}");
    assert!(text.starts_with("[!! "));
    assert!(text.contains("name {literal}"));
    assert!(msg::core_system_default_in(&cx).len() > "System default".len());
}

/// Every catalog key is called from some crate, and every `msg::` call names a key. The scan reads
/// the sources rather than trusting the compiler, so it also covers target- and feature-gated code
/// the host build never compiles. These tests do not count as a use.
#[test]
fn every_catalog_key_is_used_and_every_call_names_a_key() {
    let mut sources = Vec::new();
    let mut dirs = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("i18n/tests.rs") {
                sources.push((path.display().to_string(), std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let accessors: std::collections::BTreeMap<String, &str> =
        msg::KEYS.iter().map(|k| (k.replace('.', "_"), *k)).collect();
    let mut used = std::collections::BTreeSet::new();
    let mut unknown = Vec::new();
    for (path, text) in &sources {
        for (at, _) in text.match_indices("msg::") {
            let name: String = text[at + 5..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if name.is_empty() {
                continue;
            }
            let key = ["", "_in", "_c", "_c_in"]
                .iter()
                .find_map(|suffix| accessors.get(name.strip_suffix(suffix)?));
            match key {
                Some(key) => {
                    used.insert(*key);
                }
                None => unknown.push(format!("{path}: msg::{name}")),
            }
        }
    }
    assert!(unknown.is_empty(), "calls that name no catalog key: {unknown:#?}");
    let unused: Vec<_> = msg::KEYS.iter().filter(|k| !used.contains(*k)).collect();
    assert!(unused.is_empty(), "catalog keys no crate calls: {unused:#?}");
}
