//! End-to-end contract test for the feeder's HTTP surface: boots the real SDK
//! router (`configure_plugins` + `router`) on an ephemeral port, pointed at a
//! wiremock OpenSubtitles, and drives `/manifest`, `/health`, `/query` and
//! `/compute` over HTTP. Deterministic — fixtures were captured from the live
//! API on 2026-09-12 and trimmed (uploader names removed).
//!
//! What this pins:
//! - the manifest advertises `fileType=subtitle` — the only reason meta-search
//!   routes a `fileType:subtitle` query here;
//! - keyword and anchored queries yield persistable sidecar records (a
//!   `provider-file` `cids/` member, no content kind) that pass the gateway's
//!   own `record_matches` against the query that produced them;
//! - the upstream sees OpenSubtitles' canonical query form (sorted keys) and the
//!   API key header;
//! - a query this feeder cannot answer never reaches the upstream.

use std::collections::BTreeMap;

use meta_feeder_sdk::query_eval::record_matches;
use meta_feeder_sdk::{
    configure_plugins, router, ComputeRequest, GatewayQuery, ManifestResponse, QueryRequest,
    QueryResponse,
};
use opensubtitles_feeder::locator::decode_provider_file_cid;
use opensubtitles_feeder::plugin::OpenSubtitlesPlugin;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "test-key";

const KEYWORD: &str = include_str!("fixtures/search_keyword_matrix.json");
const MOVIE_FR: &str = include_str!("fixtures/search_imdb_133093_fr.json");
const EPISODE: &str = include_str!("fixtures/search_episode_1399_s01e01.json");

async fn spawn_feeder(api_key: &str, upstream: &MockServer) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugin = OpenSubtitlesPlugin::with_api_base(api_key, upstream.uri());
    let plugins = configure_plugins(vec![Box::new(plugin)], dir.path()).expect("configure plugins");
    let app = router(plugins, "opensubtitles-feeder-test".to_string(), dir.path());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), dir)
}

fn query(free: &str, filters: &[(&str, &str)]) -> GatewayQuery {
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

async fn post_query(base: &str, q: &GatewayQuery) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/query"))
        .json(&QueryRequest {
            upstream_id: "opensubtitles".into(),
            query: q.clone(),
            max_results: 50,
        })
        .send()
        .await
        .expect("POST /query")
}

async fn records(base: &str, q: &GatewayQuery) -> Vec<meta_feeder_sdk::DiscoveryRecord> {
    let resp = post_query(base, q).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    resp.json::<QueryResponse>().await.expect("QueryResponse").records
}

/// OpenSubtitles 301s any query string whose keys are not sorted.
struct SortedQueryKeys;

impl Match for SortedQueryKeys {
    fn matches(&self, request: &Request) -> bool {
        let keys: Vec<&str> = request
            .url
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| kv.split('=').next().unwrap_or(""))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        keys == sorted
    }
}

fn assert_persistable_sidecar(r: &meta_feeder_sdk::DiscoveryRecord, q: &GatewayQuery) {
    assert_eq!(r.fields.get("fileType").map(String::as_str), Some("subtitle"));
    assert!(!r.fields.contains_key("contentKind") && !r.fields.contains_key("domain"));
    let cid = r
        .fields
        .keys()
        .find_map(|k| k.strip_prefix("cids/"))
        .expect("a cids/ member, or the gateway never persists the record");
    let (source, id) = decode_provider_file_cid(cid).expect("a provider-file (0x100A) cid");
    assert_eq!(source, "opensubtitles");
    assert_eq!(id, format!("file:{}", r.fields["opensubtitlesid"]));
    assert!(record_matches(&r.fields, q), "gateway would drop {:?}", r.fields);
}

#[tokio::test]
async fn manifest_advertises_subtitle_only() {
    let upstream = MockServer::start().await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let m: ManifestResponse = reqwest::get(format!("{base}/manifest"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(m.plugins.len(), 1);
    assert_eq!(m.plugins[0].id, "opensubtitles");
    assert_eq!(m.plugins[0].served_file_types, vec!["subtitle".to_string()]);
    assert!(m.plugins[0].served_content_kinds.is_empty());
}

#[tokio::test]
async fn keyword_search_yields_persistable_subtitle_records() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .and(query_param("query", "matrix"))
        .and(header("Api-Key", KEY))
        .and(SortedQueryKeys)
        .respond_with(ResponseTemplate::new(200).set_body_string(KEYWORD))
        .expect(1)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let q = query("Matrix", &[("fileType", "subtitle")]);
    let recs = records(&base, &q).await;
    assert_eq!(recs.len(), 5, "one record per file");
    for r in &recs {
        assert_persistable_sidecar(r, &q);
    }
    // Ranked by download count (no hash matches in a keyword search).
    let counts: Vec<u64> = recs
        .iter()
        .map(|r| r.fields["downloadCount"].parse().unwrap())
        .collect();
    assert!(counts.windows(2).all(|w| w[0] >= w[1]), "{counts:?}");
}

#[tokio::test]
async fn movie_anchor_with_language() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .and(query_param("imdb_id", "133093"))
        .and(query_param("languages", "fr"))
        .and(SortedQueryKeys)
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE_FR))
        .expect(1)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let q = query("", &[("fileType", "subtitle"), ("imdbid", "tt0133093"), ("languages", "fre")]);
    let recs = records(&base, &q).await;
    assert!(!recs.is_empty());
    for r in &recs {
        assert_persistable_sidecar(r, &q);
        assert_eq!(r.fields.get("subtitleLanguage").map(String::as_str), Some("fre"));
        assert_eq!(r.fields.get("tmdbid").map(String::as_str), Some("603"));
    }
}

#[tokio::test]
async fn episode_anchor_is_sent_as_parent_ids() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .and(query_param("parent_tmdb_id", "1399"))
        .and(query_param("season_number", "1"))
        .and(query_param("episode_number", "1"))
        .and(SortedQueryKeys)
        .respond_with(ResponseTemplate::new(200).set_body_string(EPISODE))
        .expect(1)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let q = query(
        "",
        &[("fileType", "subtitle"), ("tmdbid", "1399"), ("season", "1"), ("episode", "1")],
    );
    let recs = records(&base, &q).await;
    assert!(!recs.is_empty());
    for r in &recs {
        assert_persistable_sidecar(r, &q);
        assert_eq!(r.fields.get("tmdbid").map(String::as_str), Some("1399"));
    }
}

#[tokio::test]
async fn unanswerable_queries_never_reach_the_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(KEYWORD))
        .expect(0)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    for q in [
        // wall/row queries — a subtitle carries none of these axes. `domain:` is
        // the case the live smoke run caught: the SDK gate only checks
        // fileType/contentKind, and only the gateway expands domain → kinds.
        query("matrix", &[("fileType", "subtitle"), ("contentKind", "movie")]),
        query("matrix", &[("fileType", "subtitle"), ("domain", "screen")]),
        query("matrix", &[("fileType", "subtitle"), ("workForm", "standalone")]),
        // never asked for subtitles
        query("matrix", &[]),
        query("matrix", &[("fileType", "video")]),
        // nothing to search on
        query("", &[("fileType", "subtitle")]),
        query("", &[("fileType", "subtitle"), ("languages", "fre")]),
        // a language OpenSubtitles does not have
        query("matrix", &[("fileType", "subtitle"), ("languages", "zzz")]),
    ] {
        assert!(records(&base, &q).await.is_empty(), "{:?}", q.filters);
    }
}

#[tokio::test]
async fn upstream_429_surfaces_as_429() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let resp = post_query(&base, &query("matrix", &[("fileType", "subtitle")])).await;
    assert_eq!(resp.status(), 429);
}

/// A 301 means the query string drifted from OpenSubtitles' canonical form.
/// It must be loud, not a silently followed extra hop on every search.
#[tokio::test]
async fn upstream_redirect_is_an_error() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("Location", "/subtitles?query=matrix"),
        )
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let resp = post_query(&base, &query("matrix", &[("fileType", "subtitle")])).await;
    assert_eq!(resp.status(), 422);
}

#[tokio::test]
async fn missing_api_key_degrades_and_answers_nothing() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(KEYWORD))
        .expect(0)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder("", &upstream).await;

    let health: serde_json::Value = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(health["status"], "ok", "{health}");

    assert!(records(&base, &query("matrix", &[("fileType", "subtitle")]))
        .await
        .is_empty());
}

#[tokio::test]
async fn compute_is_refused() {
    let upstream = MockServer::start().await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/compute"))
        .json(&ComputeRequest {
            upstream_id: "opensubtitles".into(),
            record_id: "opensubtitles:file:12658754".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
}
