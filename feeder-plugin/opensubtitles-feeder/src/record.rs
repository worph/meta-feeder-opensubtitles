//! Search result → `DiscoveryRecord`: one record per subtitle **file**.
//!
//! A subtitle is a **sidecar** (METADATA_KEYS.md §1): `fileType=subtitle`, and
//! no `contentKind` / `domain` / `workForm` — it is referenced by a work, never
//! listed on a wall.

use std::collections::BTreeMap;

use meta_feeder_sdk::plugin::upstream_id_field;
use meta_feeder_sdk::query::GatewayQuery;
use meta_feeder_sdk::types::DiscoveryRecord;

use crate::client::{FeatureDetails, OsAttributes, OsFile, OsSubtitle};
use crate::consts::{SOURCE_LABEL, UPSTREAM_ID};
use crate::lang::from_os_code;
use crate::locator::file_cid;

/// `fileType=subtitle` extensions (METADATA_KEYS.md `fileType` table). A file
/// name ending in anything else carries no `extension` rather than a guess —
/// OpenSubtitles' `format` field is `null` in practice, and many file names
/// have no extension at all (`….GPRS.fr`).
const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "sub", "sbv", "vtt", "ass", "ssa", "ttml", "dfxp", "smi"];

/// Project one search hit into a record per file. Files with no id, or whose
/// locator would not fit the 64-byte ceiling, are dropped.
pub fn project(sub: &OsSubtitle, query: &GatewayQuery) -> Vec<DiscoveryRecord> {
    let a = &sub.attributes;
    a.files
        .iter()
        .filter_map(|file| to_record(a, file, query))
        .collect()
}

fn to_record(a: &OsAttributes, file: &OsFile, query: &GatewayQuery) -> Option<DiscoveryRecord> {
    let file_id = file.file_id?;
    // ⚠ No cid, no record. The gateway persists a search record only when it
    // carries a `cids/` member, while the search-coverage gate still marks the
    // query covered — an unaddressable record means every repeat search comes
    // back empty for the coverage window.
    let cid = file_cid(file_id)?;

    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    fields.insert("fileType".into(), "subtitle".into());
    fields.insert(format!("cids/{cid}"), "true".into());
    fields.insert(upstream_id_field(UPSTREAM_ID), file_id.to_string());
    fields.insert(format!("source/{SOURCE_LABEL}"), "true".into());

    let default_fd = FeatureDetails::default();
    let fd = a.feature_details.as_ref().unwrap_or(&default_fd);

    if let Some(title) = title_for(a, fd, file) {
        fields.insert("title".into(), title);
    }
    if let Some(name) = file.file_name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        fields.insert("fileName".into(), name.to_string());
        if let Some(ext) = extension_of(name) {
            fields.insert("extension".into(), ext.to_string());
        }
    }
    if let Some(lang) = a.language.as_deref().and_then(from_os_code) {
        fields.insert(format!("languages/{lang}"), "true".into());
        fields.insert("subtitleLanguage".into(), lang);
    }

    // Ids. For an episode, the *show's* ids: meta-watch anchors a series on the
    // show's tmdbid, and `record_matches` requires the value to be equal — the
    // episode's own `tmdb_id` (63056 for GoT S01E01, not 1399) would drop every
    // record from an anchored episode search.
    let episode = is_episode(fd);
    let (imdb, tmdb) = if episode {
        (fd.parent_imdb_id, fd.parent_tmdb_id)
    } else {
        (fd.imdb_id, fd.tmdb_id)
    };
    if let Some(id) = imdb {
        fields.insert("imdbid".into(), format!("tt{id:07}"));
    }
    if let Some(id) = tmdb {
        fields.insert("tmdbid".into(), id.to_string());
    }
    if episode {
        if let Some(s) = fd.season_number {
            fields.insert("season".into(), s.to_string());
        }
        if let Some(e) = fd.episode_number {
            fields.insert("episode".into(), e.to_string());
        }
    } else if let Some(y) = fd.year {
        fields.insert("movieYear".into(), y.to_string());
    }

    // Ranking signals for the client choosing between versions.
    fields.insert(
        "downloadCount".into(),
        a.download_count.unwrap_or(0).to_string(),
    );
    fields.insert("moviehashMatch".into(), bool_str(a.moviehash_match));
    fields.insert("hearingImpaired".into(), bool_str(a.hearing_impaired));
    fields.insert(
        "machineTranslated".into(),
        bool_str(Some(
            a.ai_translated.unwrap_or(false) || a.machine_translated.unwrap_or(false),
        )),
    );

    echo_filters(&mut fields, query);

    Some(DiscoveryRecord {
        upstream_id: UPSTREAM_ID.to_string(),
        record_id: format!("{UPSTREAM_ID}:file:{file_id}"),
        fields,
    })
}

fn is_episode(fd: &FeatureDetails) -> bool {
    fd.feature_type
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("episode"))
        || fd.season_number.is_some()
        || fd.episode_number.is_some()
}

/// The release name users recognise, else the feature's display name. A
/// multi-CD subtitle gets its part number, or its files are indistinguishable.
fn title_for(a: &OsAttributes, fd: &FeatureDetails, file: &OsFile) -> Option<String> {
    let non_empty = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let base = non_empty(&a.release)
        .or_else(|| non_empty(&fd.movie_name))
        .or_else(|| non_empty(&fd.title))?;
    match (a.nb_cd, file.cd_number) {
        (Some(total), Some(n)) if total > 1 => Some(format!("{base} (CD {n}/{total})")),
        _ => Some(base),
    }
}

fn extension_of(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    SUBTITLE_EXTENSIONS.iter().copied().find(|e| *e == ext)
}

fn bool_str(b: Option<bool>) -> String {
    if b.unwrap_or(false) { "true" } else { "false" }.to_string()
}

/// Structured filters the record must echo back.
///
/// ⚠ `record_matches` (gateway + meta-search) iterates EVERY filter on the query
/// and fails any record that does not carry the key. Key-set fields (`genres`,
/// `languages`) have dedicated arms and the type axes are set explicitly, so
/// they are skipped; so is any key the record already states — echoing must
/// never overwrite a real value (`imdbid` stays the record's own).
///
/// `moviehash:` is echoed onto every record; `moviehashMatch` is what tells a
/// real hash match apart from an id match returned by the same search.
fn echo_filters(fields: &mut BTreeMap<String, String>, query: &GatewayQuery) {
    const SKIP: &[&str] = &["genres", "languages", "fileType", "type", "contentKind", "domain", "workForm"];
    for (k, values) in &query.filters {
        if SKIP.contains(&k.as_str()) || fields.contains_key(k) {
            continue;
        }
        if let Some(v) = values.first() {
            fields.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::SearchResponse;
    use crate::locator::decode_provider_file_cid;
    use meta_feeder_sdk::query_eval::record_matches;

    fn q(free: &str, filters: &[(&str, &str)]) -> GatewayQuery {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in filters {
            map.entry((*k).to_string()).or_default().push((*v).to_string());
        }
        GatewayQuery {
            raw_text: String::new(),
            free_text: free.to_string(),
            filters: map,
            ranges: Vec::new(),
            negations: Vec::new(),
        }
    }

    fn records(fixture: &str, query: &GatewayQuery) -> Vec<DiscoveryRecord> {
        let resp: SearchResponse = serde_json::from_str(fixture).unwrap();
        resp.data.iter().flat_map(|s| project(s, query)).collect()
    }

    const KEYWORD: &str = include_str!("../tests/fixtures/search_keyword_matrix.json");
    const MOVIE_FR: &str = include_str!("../tests/fixtures/search_imdb_133093_fr.json");
    const EPISODE: &str = include_str!("../tests/fixtures/search_episode_1399_s01e01.json");
    const HASH: &str = include_str!("../tests/fixtures/search_moviehash.json");

    #[test]
    fn one_addressable_sidecar_record_per_file() {
        let query = q("matrix", &[("fileType", "subtitle")]);
        let recs = records(KEYWORD, &query);
        // 4 subtitles, one of them a 2-CD release.
        assert_eq!(recs.len(), 5);
        for r in &recs {
            assert_eq!(r.fields.get("fileType").map(String::as_str), Some("subtitle"));
            for axis in ["contentKind", "domain", "workForm"] {
                assert!(!r.fields.contains_key(axis), "{axis} on a sidecar");
            }
            let cid = r
                .fields
                .keys()
                .find_map(|k| k.strip_prefix("cids/"))
                .expect("cids/ member");
            let (source, id) = decode_provider_file_cid(cid).expect("0x100A cid");
            assert_eq!(source, "opensubtitles");
            assert_eq!(id, format!("file:{}", r.fields["opensubtitlesid"]));
            assert_eq!(r.record_id, format!("opensubtitles:{id}"));
            assert!(record_matches(&r.fields, &query), "{:?}", r.fields);
        }
        let multi: Vec<_> = recs
            .iter()
            .filter(|r| r.fields.get("title").is_some_and(|t| t.contains("(CD ")))
            .collect();
        assert_eq!(multi.len(), 2, "both parts of the multi-CD subtitle are titled");
    }

    #[test]
    fn movie_anchor_with_language_matches_its_query() {
        let query = q("", &[("fileType", "subtitle"), ("imdbid", "tt0133093"), ("languages", "fre")]);
        let recs = records(MOVIE_FR, &query);
        assert!(!recs.is_empty());
        for r in &recs {
            assert_eq!(r.fields.get("subtitleLanguage").map(String::as_str), Some("fre"));
            assert_eq!(r.fields.get("languages/fre").map(String::as_str), Some("true"));
            assert_eq!(r.fields.get("imdbid").map(String::as_str), Some("tt0133093"));
            assert_eq!(r.fields.get("tmdbid").map(String::as_str), Some("603"));
            assert_eq!(r.fields.get("movieYear").map(String::as_str), Some("1999"));
            assert!(record_matches(&r.fields, &query), "{:?}", r.fields);
        }
    }

    #[test]
    fn episode_records_carry_the_show_ids() {
        let query = q(
            "",
            &[("fileType", "subtitle"), ("tmdbid", "1399"), ("season", "1"), ("episode", "1")],
        );
        let recs = records(EPISODE, &query);
        assert!(!recs.is_empty());
        for r in &recs {
            assert_eq!(r.fields.get("tmdbid").map(String::as_str), Some("1399"));
            assert_eq!(r.fields.get("imdbid").map(String::as_str), Some("tt0944947"));
            assert_eq!(r.fields.get("season").map(String::as_str), Some("1"));
            assert_eq!(r.fields.get("episode").map(String::as_str), Some("1"));
            assert!(!r.fields.contains_key("movieYear"));
            assert!(record_matches(&r.fields, &query), "{:?}", r.fields);
        }
    }

    #[test]
    fn moviehash_search_flags_matches_and_echoes_the_hash() {
        let query = q("", &[("fileType", "subtitle"), ("moviehash", "8e245d9679d31e12")]);
        let recs = records(HASH, &query);
        assert!(!recs.is_empty());
        for r in &recs {
            assert_eq!(r.fields.get("moviehashMatch").map(String::as_str), Some("true"));
            assert!(record_matches(&r.fields, &query), "{:?}", r.fields);
        }
    }

    #[test]
    fn echo_never_overwrites_a_real_value() {
        let query = q("", &[("fileType", "subtitle"), ("imdbid", "tt9999999")]);
        let recs = records(MOVIE_FR, &query);
        assert_eq!(recs[0].fields.get("imdbid").map(String::as_str), Some("tt0133093"));
        // …so a record for a different film is correctly rejected downstream.
        assert!(!record_matches(&recs[0].fields, &query));
    }

    #[test]
    fn extension_only_from_a_real_subtitle_suffix() {
        assert_eq!(extension_of("Movie.en.SRT"), Some("srt"));
        assert_eq!(extension_of("The.Matrix.GPRS.fr"), None);
        assert_eq!(extension_of("no_extension"), None);
    }
}
