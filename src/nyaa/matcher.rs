//! Pure decisions: which volumes a release adds, which variant of a release wins and which
//! files of a torrent are worth downloading. No network, no database, easy to test.

use std::collections::{HashMap, HashSet};

use super::rules::{normalize, title_matches, Parsed, Rules};
use super::torrent::TorrentInfo;

/// Volume numbers of a parsed name; a release without any counts as volume 0 ("the whole run").
pub fn volumes_of(p: &Parsed) -> Vec<u32> {
    if p.volumes.is_empty() { vec![0] } else { p.volumes.clone() }
}

/// Does a parsed release belong to the series a link follows?
pub fn matches_link(parsed: &Parsed, raw_title: &str, query: &str, exclude: &str) -> bool {
    // A file name without the series title ("Volume 01 - ….cbz") inherits the match of its torrent.
    if !parsed.title.is_empty() && !title_matches(&parsed.title, query) {
        return false;
    }
    if exclude.trim().is_empty() {
        return true;
    }
    // An invalid user regex must not silently disable the filter: treat it as "excluded".
    regex::Regex::new(exclude).map(|re| !re.is_match(raw_title)).unwrap_or(false)
}

/// Volumes a release would add on top of what is owned or already queued.
pub fn new_volumes(p: &Parsed, owned: &HashSet<u32>, queued: &HashSet<u32>) -> Vec<u32> {
    volumes_of(p).into_iter().filter(|v| !owned.contains(v) && !queued.contains(v)).collect()
}

/// From candidates that carry the same content, indexes of those to keep: one per
/// (title, volumes, chapters), chosen by the uploader's variant preference.
pub fn pick_variants(rules: &Rules, uploader: &str, candidates: &[(usize, &Parsed)]) -> HashSet<usize> {
    let mut groups: HashMap<(String, Vec<u32>, Option<(u32, u32)>), Vec<(usize, &Parsed)>> = HashMap::new();
    for (i, p) in candidates {
        groups.entry((normalize(&p.title), volumes_of(p), p.chapters)).or_default().push((*i, *p));
    }
    let mut keep = HashSet::new();
    for (_, mut group) in groups {
        group.sort_by_key(|(i, _)| *i);
        let variants: Vec<&str> = group.iter().map(|(_, p)| p.variant.as_str()).collect();
        keep.insert(group[rules.preferred_variant(uploader, &variants)].0);
    }
    keep
}

/// Files of a torrent to download: archives of the followed series that add a volume.
/// Returns `(file index, volumes it holds)`; every other index should get priority 0.
pub fn select_files(rules: &Rules, uploader: &str, query: &str, exclude: &str, torrent: &TorrentInfo, owned: &HashSet<u32>, queued: &HashSet<u32>) -> Vec<(usize, Vec<u32>)> {
    torrent
        .files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            let file_name = f.path.rsplit('/').next().unwrap_or(&f.path);
            let parsed = rules.parse(uploader, file_name).ok()?;
            if !matches_link(&parsed, file_name, query, exclude) {
                return None;
            }
            let fresh = new_volumes(&parsed, owned, queued);
            (!fresh.is_empty()).then(|| (i, volumes_of(&parsed)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nyaa::torrent::TorrentFile;

    fn rules() -> Rules {
        Rules::from_toml(include_str!("rules.toml"), None).unwrap()
    }

    fn set(v: &[u32]) -> HashSet<u32> {
        v.iter().copied().collect()
    }

    fn parse(u: &str, t: &str) -> Parsed {
        rules().parse(u, t).unwrap()
    }

    #[test]
    fn a_superseding_torrent_only_adds_the_missing_volume() {
        let p = parse("UploaderA", "Prince Freya v01-14 (2020-2026) (Digital) (UploaderA)");
        assert_eq!(new_volumes(&p, &set(&(1..=13).collect::<Vec<_>>()), &set(&[])), vec![14]);
        assert!(new_volumes(&p, &set(&(1..=14).collect::<Vec<_>>()), &set(&[])).is_empty());
        assert_eq!(new_volumes(&p, &set(&[1, 2, 3]), &set(&[4, 5])), (6..=14).collect::<Vec<_>>(), "queued volumes are not asked twice");
    }

    #[test]
    fn whole_run_releases_count_as_volume_zero() {
        let p = parse("UploaderA", "Queen's Quality (2017-2026) (Digital) (UploaderA)");
        assert_eq!(volumes_of(&p), vec![0]);
        assert_eq!(new_volumes(&p, &set(&[]), &set(&[])), vec![0]);
        assert!(new_volumes(&p, &set(&[0]), &set(&[])).is_empty());
    }

    #[test]
    fn links_match_whole_words_and_honour_the_exclude_regex() {
        let p = parse("UploaderA", "Prince Freya v01-13 (2020-2026) (Digital) (UploaderA)");
        assert!(matches_link(&p, "Prince Freya v01-13 (2020-2026) (Digital) (UploaderA)", "prince freya", ""));
        assert!(!matches_link(&p, "Prince Freya v01-13 (2020-2026) (Digital) (UploaderA)", "freya prince", ""));
        assert!(!matches_link(&p, "Prince Freya v01-13 (2020-2026) (Digital) (UploaderA)", "prince freya", "(?i)digital"));
        assert!(!matches_link(&p, "x", "prince freya", "("), "a broken exclude regex excludes instead of matching everything");
    }

    #[test]
    fn titleless_file_names_inherit_the_torrents_match() {
        // The colored Steel Ball Run torrent: no series title in any file name.
        let t = TorrentInfo {
            name: "Jojo Part 7-) Steel Ball Run".into(),
            files: ["Volume 01 - September 5th, 1890 San Diego Beach.cbz", "Volume 02 - First Stage, 15,000 Meters.cbz", "Volume 27f.cbz", "Altercolor. Vol26-Ch105.cbz", "readme.txt"]
                .iter()
                .map(|p| TorrentFile { path: (*p).into(), size: 100 })
                .collect(),
        };
        let sel = select_files(&rules(), "drgn", "steel ball run", "", &t, &set(&[1]), &set(&[]));
        assert_eq!(sel.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1, 2, 3], "volume 1 is owned, readme is not an archive");
    }

    #[test]
    fn variants_of_one_release_collapse_to_the_preferred_one() {
        let (a, b, c) = (
            parse("UploaderF", "Mitama Security - Spirit Busters (2021) (Digital) (UploaderF-repack)"),
            parse("UploaderF", "Mitama Security - Spirit Busters (2021) (Digital) (UploaderF-Empire)"),
            parse("UploaderF", "Another Series v01 (2021) (Digital) (UploaderF-repack)"),
        );
        let keep = pick_variants(&rules(), "UploaderF", &[(0, &a), (1, &b), (2, &c)]);
        assert_eq!(keep, [1usize, 2].into_iter().collect::<HashSet<_>>(), "Empire wins; the other series is unaffected");
    }

    #[test]
    fn only_new_archives_of_the_series_are_selected_from_a_torrent() {
        let file = |p: &str| TorrentFile { path: p.into(), size: 100 };
        let t = TorrentInfo {
            name: "Teppeki Honeymoon".into(),
            files: vec![
                file("Teppeki Honeymoon v01 (2025) (Digital) (UploaderE).cbz"),
                file("Teppeki Honeymoon v02 (2026) (Digital) (UploaderE).cbz"),
                file("Teppeki Honeymoon v03 (2026) (Digital) (UploaderE).epub"),
                file("cover.jpg"),
                file("Another Series v01 (2026) (Digital) (UploaderE).cbz"),
            ],
        };
        let sel = select_files(&rules(), "UploaderE", "teppeki honeymoon", "", &t, &set(&[1]), &set(&[]));
        assert_eq!(sel, vec![(1, vec![2])], "v01 is owned, the epub and jpg are not archives, the other series is not followed");
    }
}
