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
//! - a query this feeder cannot answer never reaches the upstream;
//! - redeeming a `provider-file` cid through `/compute` downloads the file on
//!   the API key alone, or as an account (re-logging in once on a stale token),
//!   returns the download-only facts as record fields, and honours the quota.

use std::collections::BTreeMap;

use base64::Engine as _;
use meta_feeder_sdk::hash::compute_ipfs_cid;
use meta_feeder_sdk::query_eval::record_matches;
use meta_feeder_sdk::{
    configure_plugins, router, ComputeRequest, ComputeResponse, GatewayQuery, HashKindDto,
    ManifestResponse, QueryRequest, QueryResponse, RedeemsResponse,
};
use opensubtitles_feeder::locator::{decode_provider_file_cid, file_cid, provider_file_cid};
use opensubtitles_feeder::plugin::OpenSubtitlesPlugin;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "test-key";
const USER: &str = "viewer";
const PASS: &str = "hunter2";
const FILE_ID: u64 = 7061834;
const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nWake up, Neo.\n";

const KEYWORD: &str = include_str!("fixtures/search_keyword_matrix.json");
const MOVIE_FR: &str = include_str!("fixtures/search_imdb_133093_fr.json");
const EPISODE: &str = include_str!("fixtures/search_episode_1399_s01e01.json");

async fn spawn_feeder(api_key: &str, upstream: &MockServer) -> (String, tempfile::TempDir) {
    spawn_plugin(OpenSubtitlesPlugin::with_api_base(api_key, upstream.uri())).await
}

async fn spawn_feeder_with_login(upstream: &MockServer) -> (String, tempfile::TempDir) {
    spawn_plugin(OpenSubtitlesPlugin::with_api_base(KEY, upstream.uri()).with_login(USER, PASS)).await
}

async fn spawn_plugin(plugin: OpenSubtitlesPlugin) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
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
    assert_eq!(m.plugins[0].package.as_deref(), Some("meta-feeder-opensubtitles"));
    // An API key alone downloads (anonymous quota), so the source is claimed.
    assert_eq!(
        serde_json::to_value(&m.plugins[0].redeems).unwrap(),
        serde_json::json!([{
            "codec": "provider-file", "field": "file", "hosts": [], "sources": ["opensubtitles"]
        }])
    );
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

// --- redeeming a provider-file locator ---------------------------------------

async fn post_compute(base: &str, record_id: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/compute"))
        .json(&ComputeRequest {
            upstream_id: "opensubtitles".into(),
            record_id: record_id.into(),
        })
        .send()
        .await
        .expect("POST /compute")
}

async fn mount_login(upstream: &MockServer, token: &str, times: Option<u64>) {
    let mock = Mock::given(method("POST"))
        .and(path("/login"))
        .and(header("Api-Key", KEY))
        .and(body_partial_json(serde_json::json!({ "username": USER, "password": PASS })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "token": token })));
    match times {
        Some(n) => mock.up_to_n_times(n).expect(n).mount(upstream).await,
        None => mock.mount(upstream).await,
    }
}

fn download_ok(upstream: &MockServer, remaining: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "link": format!("{}/files/{FILE_ID}.srt", upstream.uri()),
        "file_name": "The.Matrix.1999.en.srt",
        "requests": 1,
        "remaining": remaining,
        "reset_time_utc": "2099-01-01T00:00:00.000Z"
    }))
}

async fn mount_file(upstream: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILE_ID}.srt")))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(upstream)
        .await;
}

#[tokio::test]
async fn a_provider_file_cid_redeems_to_the_subtitle_bytes() {
    let upstream = MockServer::start().await;
    mount_login(&upstream, "t1", None).await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .and(header("Authorization", "Bearer t1"))
        .and(body_partial_json(serde_json::json!({ "file_id": FILE_ID })))
        .respond_with(download_ok(&upstream, 99))
        .expect(2)
        .mount(&upstream)
        .await;
    mount_file(&upstream, SRT).await;
    let (base, _dir) = spawn_feeder_with_login(&upstream).await;

    let resp = post_compute(&base, &file_cid(FILE_ID).unwrap()).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let body: ComputeResponse = resp.json().await.unwrap();
    assert_eq!(body.outcomes.len(), 1);
    let o = &body.outcomes[0];
    assert_eq!(o.hash_kind, HashKindDto::Sha2_256);
    assert_eq!(o.file_extension.as_deref(), Some("srt"));
    let rec = o.record.as_ref().expect("download-only facts for the locator record");
    assert_eq!(rec.fields.get("extension").map(String::as_str), Some("srt"));
    assert_eq!(rec.fields.get("fileName").map(String::as_str), Some("The.Matrix.1999.en.srt"));
    assert_eq!(rec.fields.get("sizeByte"), Some(&SRT.len().to_string()));
    assert!(
        !rec.fields.keys().any(|k| k.starts_with("cids/")),
        "a redeem never re-addresses the locator record"
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(o.bytes_b64.as_deref().unwrap())
        .unwrap();
    assert_eq!(bytes, SRT.as_bytes());
    assert_eq!(o.hash, compute_ipfs_cid(&bytes));

    // The feeder's own record id form redeems the same file.
    assert_eq!(post_compute(&base, &format!("opensubtitles:file:{FILE_ID}")).await.status(), 200);
}

#[tokio::test]
async fn a_stale_token_is_refreshed_once() {
    let upstream = MockServer::start().await;
    // First login hands out t1, the second t2.
    mount_login(&upstream, "t1", Some(1)).await;
    mount_login(&upstream, "t2", Some(1)).await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .and(header("Authorization", "Bearer t1"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .and(header("Authorization", "Bearer t2"))
        .respond_with(download_ok(&upstream, 99))
        .expect(1)
        .mount(&upstream)
        .await;
    mount_file(&upstream, SRT).await;
    let (base, _dir) = spawn_feeder_with_login(&upstream).await;

    assert_eq!(post_compute(&base, &file_cid(FILE_ID).unwrap()).await.status(), 200);
}

#[tokio::test]
async fn a_spent_quota_is_rate_limited_and_stops_calling_download() {
    let upstream = MockServer::start().await;
    mount_login(&upstream, "t1", None).await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(406).set_body_json(serde_json::json!({
            "message": "You have downloaded your allowed 20 subtitles for 24h.",
            "reset_time": "23 hours and 59 minutes",
            "reset_time_utc": "2099-01-01T00:00:00.000Z"
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder_with_login(&upstream).await;

    let cid = file_cid(FILE_ID).unwrap();
    assert_eq!(post_compute(&base, &cid).await.status(), 429);
    // Blocked locally until the reset: the upstream is not asked again.
    assert_eq!(post_compute(&base, &cid).await.status(), 429);
}

#[tokio::test]
async fn the_last_allowed_download_blocks_the_next() {
    let upstream = MockServer::start().await;
    mount_login(&upstream, "t1", None).await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .respond_with(download_ok(&upstream, 0))
        .expect(1)
        .mount(&upstream)
        .await;
    mount_file(&upstream, SRT).await;
    let (base, _dir) = spawn_feeder_with_login(&upstream).await;

    let cid = file_cid(FILE_ID).unwrap();
    assert_eq!(post_compute(&base, &cid).await.status(), 200);
    assert_eq!(post_compute(&base, &cid).await.status(), 429);
}

#[tokio::test]
async fn not_our_locator_is_not_found_and_spends_nothing() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder_with_login(&upstream).await;

    let other = provider_file_cid("otherprovider", "file:1").unwrap();
    assert_eq!(post_compute(&base, &other).await.status(), 404);
    // A card locator shares the framing but not the codec.
    assert_eq!(post_compute(&base, "bagdsaaanar2g2zdcor3duojvgq3ts").await.status(), 404);
}

#[tokio::test]
async fn an_empty_file_is_refused() {
    let upstream = MockServer::start().await;
    mount_login(&upstream, "t1", None).await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .respond_with(download_ok(&upstream, 99))
        .mount(&upstream)
        .await;
    mount_file(&upstream, "oops").await;
    let (base, _dir) = spawn_feeder_with_login(&upstream).await;

    assert_eq!(post_compute(&base, &file_cid(FILE_ID).unwrap()).await.status(), 422);
}

/// Anonymous mode sends the API key and no Bearer token.
struct NoAuthorization;

impl Match for NoAuthorization {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("authorization")
    }
}

#[tokio::test]
async fn an_api_key_alone_downloads_without_logging_in() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .and(header("Api-Key", KEY))
        .and(NoAuthorization)
        .and(body_partial_json(serde_json::json!({ "file_id": FILE_ID })))
        .respond_with(download_ok(&upstream, 4))
        .expect(1)
        .mount(&upstream)
        .await;
    mount_file(&upstream, SRT).await;
    let (base, _dir) = spawn_feeder(KEY, &upstream).await;

    let resp = post_compute(&base, &file_cid(FILE_ID).unwrap()).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let body: ComputeResponse = resp.json().await.unwrap();
    assert_eq!(body.outcomes.len(), 1);
    assert_eq!(body.outcomes[0].hash, compute_ipfs_cid(SRT.as_bytes()));

    let r: RedeemsResponse = reqwest::get(format!("{base}/redeems")).await.unwrap().json().await.unwrap();
    assert_eq!(r.redeems["opensubtitles"].len(), 1);
}

#[tokio::test]
async fn with_a_login_provider_file_is_claimed() {
    let upstream = MockServer::start().await;
    let (base, _dir) = spawn_feeder_with_login(&upstream).await;

    let r: RedeemsResponse = reqwest::get(format!("{base}/redeems")).await.unwrap().json().await.unwrap();
    assert_eq!(
        serde_json::to_value(&r.redeems["opensubtitles"]).unwrap(),
        serde_json::json!([{
            "codec": "provider-file", "field": "file", "hosts": [], "sources": ["opensubtitles"]
        }])
    );
    let m: ManifestResponse = reqwest::get(format!("{base}/manifest")).await.unwrap().json().await.unwrap();
    assert_eq!(m.plugins[0].redeems, r.redeems["opensubtitles"]);
}
