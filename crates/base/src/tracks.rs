//! **Which audio track a playback starts on**, by the viewer's language preferences, as plain
//! data. PURE.
//!
//! The settings store turns its rungs into a [`TrackPrefs`]; the media layer (`tv_media`), which
//! cannot name the screens, reads one off the playback's start and picks the track against what
//! the probe found before the Load. Both pick through this module, so the Settings page and the
//! player can never disagree about what a preference means.

/// A track's language tag (`"eng"`, `"de-DE"`, `"fre"`) as an upper-case ISO 639-1 code
/// (`"EN"`, `"DE"`, `"FR"`); empty for an untagged track. A code it does not know is its own
/// base tag upper-cased.
pub fn language_code(tag: &str) -> String {
    let base = tag.split(['-', '_']).next().unwrap_or("").trim().to_ascii_lowercase();
    language_index(&base).map_or(base, |i| LANGUAGES[i].0.to_string()).to_ascii_uppercase()
}

/// Where the language `tag` names (ISO 639-1 or -2, any case, no region) stands in [`LANGUAGES`].
pub fn language_index(tag: &str) -> Option<usize> {
    let tag = tag.trim().to_ascii_lowercase();
    LANGUAGES.iter().position(|(short, long, _)| *short == tag || long.contains(&tag.as_str()))
}

/// The language a track's tag names, in its own name (`"ger"` → `Deutsch`), or `None` for no tag,
/// `und` (undetermined) and the other non-languages. A region suffix (`pt-BR`) is read by its
/// language; an unknown code is shown as the file gives it.
pub fn language_name(tag: &str) -> Option<String> {
    let tag = tag.trim();
    let lang = tag.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    if lang.is_empty() || matches!(lang.as_str(), "und" | "unk" | "mis" | "mul" | "zxx" | "qaa") {
        return None;
    }
    Some(language_index(&lang).map_or_else(|| tag.to_string(), |i| LANGUAGES[i].2.to_string()))
}

/// **Every language the app names**: its ISO 639-1 code, the ISO 639-2 codes (bibliographic and
/// terminology, plus the odd OpenSubtitles spelling: `pob` Brazilian Portuguese, `scc` Serbian)
/// and Norwegian's 639-1 variants that mean it, and the language's own name. Data, not translated
/// text: a language is listed the way its speakers write it, as every track picker does.
pub const LANGUAGES: &[(&str, &[&str], &str)] = &[
    ("en", &["eng"], "English"),
    ("es", &["spa", "spl", "spn"], "Español"),
    ("fr", &["fre", "fra"], "Français"),
    ("de", &["ger", "deu"], "Deutsch"),
    ("it", &["ita"], "Italiano"),
    ("pt", &["por", "pob", "pom", "pb"], "Português"),
    ("ru", &["rus"], "Русский"),
    ("be", &["bel"], "Беларуская"),
    ("uk", &["ukr"], "Українська"),
    ("pl", &["pol"], "Polski"),
    ("cs", &["cze", "ces"], "Čeština"),
    ("sk", &["slo", "slk"], "Slovenčina"),
    ("hu", &["hun"], "Magyar"),
    ("ro", &["rum", "ron"], "Română"),
    ("bg", &["bul"], "Български"),
    ("sr", &["srp", "scc"], "Српски"),
    ("hr", &["hrv", "scr"], "Hrvatski"),
    ("bs", &["bos"], "Bosanski"),
    ("sl", &["slv"], "Slovenščina"),
    ("mk", &["mac", "mkd"], "Македонски"),
    ("el", &["gre", "ell"], "Ελληνικά"),
    ("tr", &["tur"], "Türkçe"),
    ("nl", &["dut", "nld"], "Nederlands"),
    ("sv", &["swe"], "Svenska"),
    ("no", &["nor", "nb", "nob", "nn", "nno"], "Norsk"),
    ("da", &["dan"], "Dansk"),
    ("fi", &["fin"], "Suomi"),
    ("is", &["ice", "isl"], "Íslenska"),
    ("et", &["est"], "Eesti"),
    ("lv", &["lav"], "Latviešu"),
    ("lt", &["lit"], "Lietuvių"),
    ("ga", &["gle"], "Gaeilge"),
    ("cy", &["wel", "cym"], "Cymraeg"),
    ("ca", &["cat"], "Català"),
    ("eu", &["baq", "eus"], "Euskara"),
    ("gl", &["glg"], "Galego"),
    ("ar", &["ara"], "العربية"),
    ("he", &["heb"], "עברית"),
    ("fa", &["per", "fas"], "فارسی"),
    ("ur", &["urd"], "اردو"),
    ("hi", &["hin"], "हिन्दी"),
    ("bn", &["ben"], "বাংলা"),
    ("ta", &["tam"], "தமிழ்"),
    ("te", &["tel"], "తెలుగు"),
    ("ml", &["mal"], "മലയാളം"),
    ("kn", &["kan"], "ಕನ್ನಡ"),
    ("mr", &["mar"], "मराठी"),
    ("pa", &["pan"], "ਪੰਜਾਬੀ"),
    ("th", &["tha"], "ไทย"),
    ("vi", &["vie"], "Tiếng Việt"),
    ("id", &["ind"], "Bahasa Indonesia"),
    ("ms", &["may", "msa"], "Bahasa Melayu"),
    ("tl", &["tgl", "fil"], "Filipino"),
    ("zh", &["chi", "zho", "zht", "zhe", "chs", "cht"], "中文"),
    ("ja", &["jpn"], "日本語"),
    ("ko", &["kor"], "한국어"),
    ("ka", &["geo", "kat"], "ქართული"),
    ("hy", &["arm", "hye"], "Հայերեն"),
    ("az", &["aze"], "Azərbaycan"),
    ("kk", &["kaz"], "Қазақ"),
    ("uz", &["uzb"], "Oʻzbek"),
    ("sq", &["alb", "sqi"], "Shqip"),
    ("af", &["afr"], "Afrikaans"),
    ("sw", &["swa"], "Kiswahili"),
    ("la", &["lat"], "Latina"),
];

/// The languages a preference list offers to add, in the order the picker lists them: the common
/// languages of audio tracks, as [`language_code`] codes.
pub const COMMON: [&str; 33] = [
    "EN", "DE", "FR", "ES", "IT", "PT", "RU", "UK", "PL", "NL", "SV", "NO", "DA", "FI", "CS", "SK", "HU", "RO",
    "BG", "EL", "TR", "HR", "SR", "AR", "HE", "HI", "JA", "KO", "ZH", "TH", "VI", "ID", "BE",
];

/// One audio track as the pick reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track<'a> {
    /// The container's language tag (ISO 639-1 or -2, maybe with a region); `""` when untagged.
    pub language: &'a str,
    /// The file's own default flag.
    pub default: bool,
}

/// The viewer's track preferences, as [`language_code`] codes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackPrefs {
    /// The audio languages, first preferred first; empty plays the file's default track.
    pub audio: Vec<String>,
}

impl TrackPrefs {
    /// Where `tag` stands in the audio order: 0 the first preferred language; `audio.len()` (last)
    /// for a language not in it, and for every track while audio follows the file.
    pub fn audio_rank(&self, tag: &str) -> usize {
        let code = language_code(tag);
        self.audio.iter().position(|l| *l == code).unwrap_or(self.audio.len())
    }

    /// The audio track: the first in the first preferred language that has one, else the file's
    /// default track, else the first.
    pub fn audio(&self, audio: &[Track<'_>]) -> Option<usize> {
        self.audio
            .iter()
            .find_map(|l| audio.iter().position(|t| language_code(t.language) == *l))
            .or_else(|| audio.iter().position(|t| t.default))
            .or((!audio.is_empty()).then_some(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(language: &str, default: bool) -> Track<'_> {
        Track { language, default }
    }

    #[test]
    fn the_audio_pick_follows_the_order_then_the_default() {
        let p = TrackPrefs { audio: vec!["PL".into(), "RU".into()] };
        assert_eq!(p.audio(&[t("eng", true), t("rus", false), t("pol", false)]), Some(2), "the first preferred language");
        assert_eq!(p.audio(&[t("eng", false), t("rus", false)]), Some(1), "then the next one");
        assert_eq!(p.audio(&[t("eng", false), t("ger", true)]), Some(1), "no preferred language: the file's default");
        assert_eq!(p.audio(&[t("eng", false), t("ger", false)]), Some(0), "else the first");
        assert_eq!(p.audio(&[]), None);
        assert_eq!((p.audio_rank("pol"), p.audio_rank("ru-RU"), p.audio_rank("ger")), (0, 1, 2));
        assert_eq!(TrackPrefs::default().audio_rank("ger"), 0, "no audio order: every track ranks last (0 of 0)");
    }

    #[test]
    fn every_spelling_of_a_language_is_its_one_code() {
        for (tag, code) in [("ger", "DE"), ("deu", "DE"), ("cze", "CS"), ("pob", "PT"), ("pt-BR", "PT"), ("nob", "NO"), ("scc", "SR"), ("chi", "ZH"), ("EL", "EL"), ("tlh", "TLH")] {
            assert_eq!(language_code(tag), code, "{tag}");
        }
        assert_eq!(language_name("cze").as_deref(), Some("Čeština"));
        assert!(COMMON.iter().all(|c| language_index(c).is_some() && language_code(c) == *c), "every common language is named");
    }
}
