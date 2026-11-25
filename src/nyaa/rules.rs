//! Release title / archive file name rules (PLAN.md → Nyaa → "Matching and number extraction").

use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;

const DEFAULT_RULES: &str = include_str!("rules.toml");
/// Sanity cap so a typo like `v1-99999` cannot allocate a huge list.
const MAX_VOLUME: u32 = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Volumes (`v01-13`), possibly with loose chapters.
    Volumes,
    /// Chapters compiled into volumes (`001-021 as v01-05`).
    Compilation,
    /// No volume number: the whole run or a one-shot.
    Run,
}

/// Colour editions of a series are separate downloads (same volume numbers, different files).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Edition {
    #[default]
    Standard,
    Colored,
}

impl Edition {
    pub fn as_str(self) -> &'static str {
        match self {
            Edition::Standard => "standard",
            Edition::Colored => "colored",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s == "colored" { Edition::Colored } else { Edition::Standard }
    }
}

/// "Colored", "(Colour)", "Full Color", "Digital Colored" as a word of its own. A hyphenated part of a
/// title ("Seven-Colored Memories") does not count.
pub fn detect_edition(name: &str) -> Edition {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)(?:^|[\s(\[])(?:colou?red|full[ -]?colou?r(?:ed)?|digital colou?r(?:ed)?)(?:[\s)\]]|$)|[(\[]\s*colou?r\s*[)\]]").unwrap());
    if re.is_match(name) { Edition::Colored } else { Edition::Standard }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    /// Empty for file names that do not carry the series title ("Volume 01 - Chapter name.cbz").
    pub title: String,
    pub edition: Edition,
    pub volumes: Vec<u32>,
    pub chapters: Option<(u32, u32)>,
    /// Text of the last `(…)`/`[…]` group: release group(s) and variant tags.
    pub variant: String,
    pub kind: Kind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    /// Not an archive (EPUB, audio, …).
    Extension,
    /// Matched a `reject_title` rule.
    Rejected,
    /// No pattern matched.
    Unparsed,
}

#[derive(Deserialize, Default)]
struct RawUploader {
    accept_ext: Option<Vec<String>>,
    reject_title: Option<Vec<String>>,
    patterns: Option<Vec<String>>,
    prefer_variant: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct RawRules {
    #[serde(default)]
    default: RawUploader,
    #[serde(default)]
    uploader: HashMap<String, RawUploader>,
}

struct UploaderRules {
    accept_ext: Vec<String>,
    reject_title: Vec<Regex>,
    patterns: Vec<Regex>,
    prefer_variant: Vec<String>,
}

pub struct Rules {
    default: UploaderRules,
    uploaders: HashMap<String, UploaderRules>,
}

fn compile(list: &[String]) -> Result<Vec<Regex>, String> {
    list.iter().map(|p| Regex::new(p).map_err(|e| format!("bad rule {p:?}: {e}"))).collect()
}

fn build(raw: &RawUploader, base: Option<&UploaderRules>) -> Result<UploaderRules, String> {
    let pick = |own: &Option<Vec<String>>, f: fn(&UploaderRules) -> Vec<String>| own.clone().or_else(|| base.map(f)).unwrap_or_default();
    Ok(UploaderRules {
        accept_ext: pick(&raw.accept_ext, |b| b.accept_ext.clone()),
        reject_title: match (&raw.reject_title, base) {
            (Some(own), _) => compile(own)?,
            (None, Some(b)) => b.reject_title.clone(),
            (None, None) => Vec::new(),
        },
        patterns: match (&raw.patterns, base) {
            (Some(own), _) => compile(own)?,
            (None, Some(b)) => b.patterns.clone(),
            (None, None) => Vec::new(),
        },
        prefer_variant: pick(&raw.prefer_variant, |b| b.prefer_variant.clone()),
    })
}

impl Rules {
    /// Built-in rules, with `~/.config/nezuyomi/nyaa-rules.toml` layered on top if it exists.
    pub fn load() -> Self {
        let user = crate::paths::config_dir().join("nyaa-rules.toml");
        let user_text = std::fs::read_to_string(&user).ok();
        match Self::from_toml(DEFAULT_RULES, user_text.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[nyaa] {}: {e}; using the built-in rules", user.display());
                Self::from_toml(DEFAULT_RULES, None).expect("built-in nyaa rules are valid")
            }
        }
    }

    pub fn from_toml(defaults: &str, user: Option<&str>) -> Result<Self, String> {
        let base: RawRules = toml::from_str(defaults).map_err(|e| e.to_string())?;
        let user: RawRules = user.map(|u| toml::from_str(u).map_err(|e| e.to_string())).transpose()?.unwrap_or_default();
        let default = build(&user.default, Some(&build(&base.default, None)?))?;
        let mut uploaders = HashMap::new();
        let names: std::collections::BTreeSet<&String> = base.uploader.keys().chain(user.uploader.keys()).collect();
        for name in names {
            let merged = match (user.uploader.get(name), base.uploader.get(name)) {
                (Some(u), _) => build(u, Some(&default))?,
                (None, Some(b)) => build(b, Some(&default))?,
                (None, None) => continue,
            };
            uploaders.insert(name.clone(), merged);
        }
        Ok(Self { default, uploaders })
    }

    fn for_uploader(&self, uploader: &str) -> &UploaderRules {
        self.uploaders.get(uploader).unwrap_or(&self.default)
    }

    /// Parses a torrent title (no extension) or an archive file name (with extension).
    pub fn parse(&self, uploader: &str, name: &str) -> Result<Parsed, Skip> {
        let rules = self.for_uploader(uploader);
        let (stem, ext) = split_ext(name);
        if let Some(ext) = ext {
            if !rules.accept_ext.iter().any(|a| a.eq_ignore_ascii_case(ext)) {
                return Err(Skip::Extension);
            }
        }
        if rules.reject_title.iter().any(|r| r.is_match(stem)) {
            return Err(Skip::Rejected);
        }
        for pattern in &rules.patterns {
            let Some(c) = pattern.captures(stem) else { continue };
            let num = |n: &str| c.name(n).and_then(|m| m.as_str().parse::<u32>().ok());
            let volumes = match (num("vol"), num("vol_end")) {
                (Some(a), Some(b)) if a <= b && b <= MAX_VOLUME => (a..=b).collect(),
                (Some(a), None) if a <= MAX_VOLUME => vec![a],
                (Some(_), _) => continue,
                (None, _) => Vec::new(),
            };
            let chapters = match (num("ch"), num("ch_end")) {
                (Some(a), Some(b)) => Some((a, b)),
                (Some(a), None) => Some((a, a)),
                _ => None,
            };
            let kind = if volumes.is_empty() {
                Kind::Run
            } else if stem.contains(" as v") && chapters.is_some() {
                Kind::Compilation
            } else {
                Kind::Volumes
            };
            let title = c.name("title").map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            return Ok(Parsed { title, edition: detect_edition(stem), volumes, chapters, variant: last_group(stem), kind });
        }
        Err(Skip::Unparsed)
    }

    /// Of several variants of one release (same title and volumes) the index of the preferred one.
    pub fn preferred_variant(&self, uploader: &str, variants: &[&str]) -> usize {
        let prefs = &self.for_uploader(uploader).prefer_variant;
        for pref in prefs {
            if let Some(i) = variants.iter().position(|v| v.to_lowercase().contains(&pref.to_lowercase())) {
                return i;
            }
        }
        0
    }
}

const KNOWN_EXTS: [&str; 9] = ["cbz", "zip", "cbr", "rar", "7z", "epub", "pdf", "mp3", "m4b"];

fn split_ext(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if KNOWN_EXTS.contains(&ext.to_ascii_lowercase().as_str()) => (stem, Some(ext)),
        _ => (name, None),
    }
}

/// Text of the last `( … )` or `[ … ]` group of a name.
fn last_group(stem: &str) -> String {
    static GROUP: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = GROUP.get_or_init(|| Regex::new(r"[\(\[]([^()\[\]]*)[\)\]]\s*(?:\([A-Za-z]{1,3}\))?\s*$").unwrap());
    re.captures(stem).map(|c| c[1].trim().to_string()).unwrap_or_default()
}

/// Lower-case alphanumeric words, `&` as "and", without a leading "the": for comparing
/// a release title with the phrase a series is linked with.
pub fn normalize(s: &str) -> String {
    let lower = s.to_lowercase().replace('&', " and ");
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let words = if words.first() == Some(&"the") { &words[1..] } else { &words[..] };
    words.join(" ")
}

/// Does `title` contain the (normalised) phrase as whole words?
pub fn title_matches(title: &str, phrase: &str) -> bool {
    let (t, p) = (normalize(title), normalize(phrase));
    !p.is_empty() && format!(" {t} ").contains(&format!(" {p} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        // The shipped defaults name no uploader; variant preference comes from a user table.
        Rules::from_toml(DEFAULT_RULES, Some("[uploader.tanuki]\nprefer_variant = [\"Crown\", \"repack\"]\n")).unwrap()
    }

    fn vols(r: &Parsed) -> Vec<u32> {
        r.volumes.clone()
    }

    #[test]
    fn volume_ranges() {
        let r = rules().parse("Zenith", "Duke Fenrir v01-13 (2020-2026) (Digital) (Zenith)").unwrap();
        assert_eq!((r.title.as_str(), vols(&r), r.kind), ("Duke Fenrir", (1..=13).collect::<Vec<_>>(), Kind::Volumes));
        assert_eq!(r.variant, "Zenith");
        let r = rules().parse("bluelark", "How to Tame My Moody Sister v01 (2026) (Digital) (Lark)").unwrap();
        assert_eq!((r.title.as_str(), vols(&r)), ("How to Tame My Moody Sister", vec![1]));
    }

    #[test]
    fn volumes_with_loose_chapters() {
        let r = rules().parse("Zenith", "Grand Summoner Haruto v01,009-090 (2024-2026) (Digital) (Zenith)").unwrap();
        assert_eq!((vols(&r), r.chapters), (vec![1], Some((9, 90))));
        let r = rules().parse("Zenith", "Mega Psychic Officer Kojo v01-03,026-065 (2024-2026) (Digital) (Zenith)").unwrap();
        assert_eq!((vols(&r), r.chapters), (vec![1, 2, 3], Some((26, 65))));
    }

    #[test]
    fn whole_run_without_volume_number() {
        let r = rules().parse("Zenith", "Empress Quality (2017-2026) (Digital) (Zenith)").unwrap();
        assert_eq!((r.title.as_str(), r.kind, vols(&r)), ("Empress Quality", Kind::Run, vec![]));
    }

    #[test]
    fn compilation_releases() {
        let r = rules()
            .parse("PixelScans", "The Star-Slaying Dragon King 001-021 as v01-05 (Digital-Compilation) (Acme Comics) (PixelScans)")
            .unwrap();
        assert_eq!((r.title.as_str(), r.kind, vols(&r), r.chapters), ("The Star-Slaying Dragon King", Kind::Compilation, (1..=5).collect::<Vec<_>>(), Some((1, 21))));
    }

    #[test]
    fn bracket_style_and_several_groups() {
        let r = rules().parse("Mapleton", "Reborn Alchemist v01-02 [Hana Press] [Maple]").unwrap();
        assert_eq!((r.title.as_str(), vols(&r), r.variant.as_str()), ("Reborn Alchemist", vec![1, 2], "Maple"));
        let r = rules().parse("bluelark", "Nameless Echo v01-08 (2022-2026) (Digital) (Irontail, Lark)").unwrap();
        assert_eq!((vols(&r).len(), r.variant.as_str()), (8, "Irontail, Lark"));
    }

    #[test]
    fn file_names_inside_torrents() {
        let r = rules().parse("PixelScans", "Ironclad Honeymoon v02 (2026) (Digital) (PixelScans).cbz").unwrap();
        assert_eq!((r.title.as_str(), vols(&r)), ("Ironclad Honeymoon", vec![2]));
        // a trailing "(F)" flag after the group is ignored for the variant
        let r = rules().parse("bluelark", "How to Tame My Moody Sister v01 (2026) (Digital) (Lark) (F).cbz").unwrap();
        assert_eq!(r.variant, "Lark");
    }

    #[test]
    fn non_manga_is_skipped() {
        let r = rules();
        assert_eq!(r.parse("Mapleton", "Hermit Life in Another Realm v01-14 [Audiobook] [Nine Seas Siren] [Stick & Maple]"), Err(Skip::Rejected));
        assert_eq!(r.parse("tanuki", "The Wanderer, Exploring the Cave v01 [Foo Press] [tanuki-Crown].epub"), Err(Skip::Extension));
        assert_eq!(r.parse("Brightfox", "Weekly Foo, Bar and Baz Chapter Updates - Week 47 2026 (Digital) (Brightfox)"), Err(Skip::Rejected));
    }

    #[test]
    fn jpeg_xl_and_mass_packs_are_rejected() {
        let r = rules();
        assert_eq!(r.parse("Mapleton", "The Poor Heiress Wants to Marry the Haunted Baron v01 [JPEG-XL] [Maple]"), Err(Skip::Rejected));
        assert_eq!(r.parse("Mapleton", "Full Beta Manga App Rip up to 2026-11-18 [Anon]"), Err(Skip::Rejected));
        assert!(r.parse("Mapleton", "The Poor Heiress Wants to Marry the Haunted Baron v01 [Maple]").is_ok());
    }

    #[test]
    fn volume_style_file_names_without_a_series_title() {
        let r = rules();
        let p = r.parse("dragonet", "Volume 01 - June 5th, 1850 Harbor Town.cbz").unwrap();
        assert_eq!((p.title.as_str(), vols(&p)), ("", vec![1]));
        assert_eq!(vols(&r.parse("dragonet", "Volume 27f.cbz").unwrap()), vec![27]);
        assert_eq!(vols(&r.parse("dragonet", "Vol. 3.cbz").unwrap()), vec![3]);
        assert_eq!(vols(&r.parse("dragonet", "Altershade. Vol26-Ch105.cbz").unwrap()), vec![26]);
        // Real series-style names still win and keep their title.
        assert_eq!(r.parse("Zenith", "Duke Fenrir v01 (2020) (Digital) (Zenith).cbz").unwrap().title, "Duke Fenrir");
    }

    #[test]
    fn colored_editions_are_recognised_as_a_word() {
        let r = rules();
        let c = r.parse("dragonet", "Wild Quest Adventure Part 7 - Iron Orb Race Colored (Single Pages)").unwrap();
        assert_eq!((c.edition, c.kind), (Edition::Colored, Kind::Run));
        assert!(c.title.contains("Iron Orb Race"));
        assert_eq!(r.parse("Brightfox", "Iron Orb Race v01-07 (2025-2026) (Omnibus Edition) (Digital) (Brightfox)").unwrap().edition, Edition::Standard);
        assert_eq!(detect_edition("Stone Doctor v01 (Digital Colored) (Group)"), Edition::Colored);
        assert_eq!(detect_edition("Stone Doctor v01 (Full Color) (Group)"), Edition::Colored);
        assert_eq!(detect_edition("Series v01 [Colour] [Zenith]"), Edition::Colored);
        assert_eq!(detect_edition("The Seven Hidden Virtues v02 - Seven-Colored Memories [Vertex] [Zenith]"), Edition::Standard, "a hyphenated word of the title");
        assert_eq!(detect_edition("Watercolor Days v01"), Edition::Standard);
        assert_eq!(detect_edition("The Color of Happiness v01 (Digital) (Group)"), Edition::Standard, "a plain word 'Color' outside brackets");
    }

    #[test]
    fn tanuki_prefers_crown_then_repack() {
        let r = rules();
        let v = ["tanuki-repack", "tanuki-Crown+anavel"];
        assert_eq!(r.preferred_variant("tanuki", &v), 1);
        assert_eq!(r.preferred_variant("tanuki", &["tanuki-repack"]), 0);
        assert_eq!(r.preferred_variant("Zenith", &v), 0, "uploaders without a preference keep the first");
    }

    #[test]
    fn user_rules_override_one_uploader_only() {
        let user = "[uploader.tanuki]\nprefer_variant = [\"repack\"]\n";
        let r = Rules::from_toml(DEFAULT_RULES, Some(user)).unwrap();
        assert_eq!(r.preferred_variant("tanuki", &["tanuki-repack", "tanuki-Crown"]), 0);
        assert!(r.parse("Zenith", "Duke Fenrir v01-13 (2020-2026) (Digital) (Zenith)").is_ok());
        assert!(Rules::from_toml(DEFAULT_RULES, Some("[default]\npatterns = ['(']\n")).is_err());
    }

    #[test]
    fn linking_phrase_matches_whole_words_ignoring_case_and_punctuation() {
        assert!(title_matches("Duke Fenrir v01-13 (2020-2026)", "duke fenrir"));
        assert!(title_matches("The Knight and the Mage, Reborn and Betrothed 001-039", "knight & the mage"));
        assert!(title_matches("Mogu-kun - God of Creation v01", "Mogu kun"));
        assert!(!title_matches("Duke Fenrirr v01", "duke fenrir"));
        assert!(!title_matches("Anything", ""));
    }

    #[test]
    fn absurd_volume_numbers_are_not_expanded() {
        assert!(rules().parse("Zenith", "Typo v1-99999 (2020) (Digital) (Zenith)").is_err() || rules().parse("Zenith", "Typo v1-99999 (2020) (Digital) (Zenith)").unwrap().volumes.len() < 2001);
    }
}
