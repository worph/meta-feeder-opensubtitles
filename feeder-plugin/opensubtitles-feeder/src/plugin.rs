//! `OpenSubtitlesPlugin` — the `FeederPlugin` for `fileType:subtitle`.
//!
//! Two query shapes, both answered; which one to send is the client's call:
//!
//! - **keyword** — `matrix fileType:subtitle` → `query=matrix`
//! - **anchored** — any of `imdbid:` `tmdbid:` `season:` `episode:`
//!   `movieYear:` `moviehash:`, e.g. `tmdbid:1399 season:1 episode:1 fileType:subtitle`
//!
//! plus an optional `languages:` narrowing on either.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use meta_feeder_sdk::budget::RateBudget;
use meta_feeder_sdk::common::open_midhash_cache;
use meta_feeder_sdk::config::{ConfigField as F, ConfigSchema};
use meta_feeder_sdk::hash::compute_ipfs_cid;
use meta_feeder_sdk::plugin::{ConfigError, FeederPlugin, HashKind, HashOutcome, RedeemClaim};
use meta_feeder_sdk::query::GatewayQuery;
use meta_feeder_sdk::query_eval::query_accepts_plugin;
use meta_feeder_sdk::types::{DiscoveryRecord, GatewayError, Hash, PluginHealth};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::client::{OsClient, SearchParams};
use crate::consts::{
    DEFAULT_SUBTITLE_EXTENSION, DEFAULT_USER_AGENT, ENV_API_KEY, ENV_PASSWORD, ENV_USERNAME,
    ENV_USER_AGENT, MIN_SUBTITLE_BYTES, OS_API_BASE, OS_BURST, OS_RATE_PER_SEC, PACKAGE,
    UPSTREAM_ID,
};
use crate::lang::to_os_codes;
use crate::locator::file_id_of;
use crate::record::{extension_of, project};

/// Keys match [`FeederPlugin::config_schema`], which is what the SDK writes to
/// `config.json` from the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OsConfig {
    #[serde(default, rename = "apiKey")]
    api_key: String,
    #[serde(default, rename = "userAgent")]
    user_agent: String,
    /// Optional account login for `/download`. Blank → downloads run on the API
    /// key alone (anonymous quota).
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

pub struct OpenSubtitlesPlugin {
    config: OsConfig,
    api_base: String,
    /// `None` until `configure` runs, and stays `None` without an API key — the
    /// soft-skip path: `/health` says why, queries return nothing.
    client: Option<OsClient>,
}

impl OpenSubtitlesPlugin {
    /// Production constructor: env seeds, real endpoint.
    pub fn from_env() -> Self {
        Self {
            config: OsConfig {
                api_key: std::env::var(ENV_API_KEY).unwrap_or_default(),
                user_agent: std::env::var(ENV_USER_AGENT).unwrap_or_default(),
                username: std::env::var(ENV_USERNAME).unwrap_or_default(),
                password: std::env::var(ENV_PASSWORD).unwrap_or_default(),
            },
            api_base: OS_API_BASE.to_string(),
            client: None,
        }
    }

    /// Test constructor: a fixed key and an endpoint override (wiremock).
    pub fn with_api_base(api_key: &str, api_base: String) -> Self {
        Self {
            config: OsConfig {
                api_key: api_key.to_string(),
                ..OsConfig::default()
            },
            api_base,
            client: None,
        }
    }

    /// Test builder: an account login (downloads use the account's quota).
    pub fn with_login(mut self, username: &str, password: &str) -> Self {
        self.config.username = username.to_string();
        self.config.password = password.to_string();
        self
    }

    /// File wins over env, like every other feeder: `config.json` in the
    /// per-plugin cache dir, read at `configure` time. No hot reload.
    fn load_config(&mut self, cache_dir: &Path) {
        let Ok(bytes) = std::fs::read(cache_dir.join("config.json")) else {
            return;
        };
        let Ok(file_cfg) = serde_json::from_slice::<OsConfig>(&bytes) else {
            warn!(target: "opensubtitles", "config.json is not valid; keeping env seeds");
            return;
        };
        if !file_cfg.api_key.trim().is_empty() {
            self.config.api_key = file_cfg.api_key;
        }
        if !file_cfg.user_agent.trim().is_empty() {
            self.config.user_agent = file_cfg.user_agent;
        }
        if !file_cfg.username.trim().is_empty() {
            self.config.username = file_cfg.username;
        }
        if !file_cfg.password.trim().is_empty() {
            self.config.password = file_cfg.password;
        }
    }
}

/// Axes a sidecar never carries (METADATA_KEYS.md §1).
const NON_SIDECAR_AXES: &[&str] = &["contentKind", "domain", "workForm"];

/// True when the query explicitly asks for subtitles, and for nothing a
/// subtitle record can't carry.
///
/// - `query_accepts_plugin` alone admits a query with no type filter at all, and
///   a query that never mentioned subtitles is not a subtitle question.
/// - ⚠ It also checks only the `fileType` / `contentKind` axes. A `domain:` or
///   `workForm:` filter is a wall or row query that no subtitle record can
///   satisfy (`record_matches` drops every row lacking the key). The gateway
///   rewrites `domain:` into `contentKind:` on its own dispatch path, but a
///   direct `/query` sees the raw filter and would spend an upstream call on a
///   guaranteed-empty answer — the live smoke run returned 10 rows for
///   `domain:screen` before this check existed.
fn requests_subtitles(query: &GatewayQuery) -> bool {
    if NON_SIDECAR_AXES.iter().any(|k| query.filters.contains_key(*k)) {
        return false;
    }
    ["fileType", "type"].iter().any(|k| {
        query
            .filters
            .get(*k)
            .is_some_and(|vs| vs.iter().any(|v| v.eq_ignore_ascii_case("subtitle")))
    })
}

fn first_filter<'a>(query: &'a GatewayQuery, key: &str) -> Option<&'a str> {
    query
        .filters
        .get(key)
        .and_then(|vs| vs.iter().map(|v| v.trim()).find(|v| !v.is_empty()))
}

fn parse_u64(v: Option<&str>) -> Option<u64> {
    v.and_then(|s| s.trim().parse().ok())
}

/// Resolve a gateway query into one OpenSubtitles search. `None` means "no
/// upstream call can answer this": nothing to search on, or a `languages:`
/// filter naming only languages OpenSubtitles does not have.
pub fn search_params(query: &GatewayQuery) -> Option<SearchParams> {
    let mut p = SearchParams {
        query: Some(query.free_text.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        imdb_id: first_filter(query, "imdbid").and_then(|v| {
            let digits = v
                .strip_prefix("tt")
                .or_else(|| v.strip_prefix("TT"))
                .unwrap_or(v);
            digits.parse().ok()
        }),
        tmdb_id: parse_u64(first_filter(query, "tmdbid")),
        season: parse_u64(first_filter(query, "season")),
        episode: parse_u64(first_filter(query, "episode")),
        year: parse_u64(first_filter(query, "movieYear")),
        moviehash: first_filter(query, "moviehash")
            .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase),
        languages: Vec::new(),
    };

    if let Some(values) = query.filters.get("languages") {
        p.languages = values
            .iter()
            .flat_map(|v| v.split(','))
            .flat_map(to_os_codes)
            .collect();
        if p.languages.is_empty() {
            // The client asked for languages we cannot express. Searching
            // without the filter would return only rows `record_matches` drops.
            return None;
        }
    }

    (!p.is_unbounded()).then_some(p)
}

fn rank_key(r: &DiscoveryRecord) -> (bool, u64) {
    (
        r.fields.get("moviehashMatch").is_some_and(|v| v == "true"),
        r.fields
            .get("downloadCount")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    )
}

#[async_trait]
impl FeederPlugin for OpenSubtitlesPlugin {
    fn upstream_id(&self) -> &'static str {
        UPSTREAM_ID
    }

    /// ⚠ `subtitle` only, and **no content kinds**: a subtitle is a sidecar
    /// with no `contentKind`/`domain` (METADATA_KEYS.md §1), so a wall or row
    /// query (`domain:screen`, `contentKind:movie`) is correctly not routed here.
    fn served_file_types(&self) -> &'static [&'static str] {
        &["subtitle"]
    }

    fn served_content_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn configure(&mut self, cache_dir: &Path) -> Result<(), ConfigError> {
        self.load_config(cache_dir);
        let api_key = self.config.api_key.trim().to_string();
        if api_key.is_empty() {
            warn!(
                target: "opensubtitles",
                "no OpenSubtitles API key (config page or {ENV_API_KEY}); subtitle searches return nothing"
            );
            return Ok(());
        }
        let cache = open_midhash_cache(cache_dir, UPSTREAM_ID)?;
        let user_agent = match self.config.user_agent.trim() {
            "" => DEFAULT_USER_AGENT,
            ua => ua,
        };
        let client = OsClient::new(
            self.api_base.clone(),
            api_key,
            user_agent,
            RateBudget::new(OS_RATE_PER_SEC, OS_BURST),
            Some(cache),
        )
        .with_login(&self.config.username, &self.config.password);
        if !client.has_login() {
            info!(
                target: "opensubtitles",
                "no OpenSubtitles account login ({ENV_USERNAME}/{ENV_PASSWORD} or config page); \
                 subtitle downloads use the API key's anonymous quota"
            );
        }
        self.client = Some(client);
        Ok(())
    }

    fn health(&self) -> PluginHealth {
        if self.client.is_some() {
            PluginHealth::Ok
        } else {
            PluginHealth::Degraded {
                reason: "no OpenSubtitles API key configured".into(),
            }
        }
    }

    async fn handle_query(
        &self,
        query: &GatewayQuery,
        max_results: usize,
    ) -> Result<Vec<DiscoveryRecord>, GatewayError> {
        if !requests_subtitles(query)
            || !query_accepts_plugin(query, self.served_file_types(), self.served_content_kinds())
        {
            return Ok(Vec::new());
        }
        let Some(client) = self.client.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(params) = search_params(query) else {
            debug!(target: "opensubtitles", filters = ?query.filters.keys().collect::<Vec<_>>(), "nothing searchable in query");
            return Ok(Vec::new());
        };

        let subtitles = client.search(&params).await?;
        let mut records: Vec<DiscoveryRecord> =
            subtitles.iter().flat_map(|s| project(s, query)).collect();
        // Hash matches first (they are the right sync for the file in hand),
        // then popularity; record_id keeps the order deterministic.
        records.sort_by(|a, b| {
            rank_key(b)
                .cmp(&rank_key(a))
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
        records.truncate(max_results);
        Ok(records)
    }

    /// **Redeem** a `provider-file` locator: download the subtitle file it names.
    ///
    /// `record_id` is the `0x100A` cid (what the gateway's redeem route sends)
    /// or this feeder's `opensubtitles:file:<id>` record id. Anything else —
    /// including another source's `provider-file` — is `NotFound`: not ours.
    ///
    /// ⚠ Spends one unit of the daily download quota (the API key's anonymous
    /// quota, or the account's with a login). Returns ONE `Sha2_256` outcome
    /// with the bytes and a record of what only the download knows —
    /// `extension`, `fileName`, `sizeByte`. The gateway merges those onto the
    /// locator record next to the `file` pointer it writes; everything else
    /// (title, language, ids) is already there from the search.
    async fn compute_outcomes(&self, record_id: &str) -> Result<Vec<HashOutcome>, GatewayError> {
        let Some(file_id) = file_id_of(record_id) else {
            return Err(GatewayError::NotFound);
        };
        let Some(client) = self.client.as_ref() else {
            return Err(GatewayError::Permanent(
                "opensubtitles: no API key configured".into(),
            ));
        };

        info!(
            target: "opensubtitles",
            file_id,
            login = client.has_login(),
            "downloading subtitle (spends download quota)"
        );
        let got = client.download(file_id).await?;
        if got.bytes.len() < MIN_SUBTITLE_BYTES {
            return Err(GatewayError::Permanent(format!(
                "opensubtitles: file {file_id} is {} bytes — not a subtitle",
                got.bytes.len()
            )));
        }
        if let Some(remaining) = got.remaining {
            debug!(target: "opensubtitles", file_id, remaining, "download quota remaining");
        }
        let extension = got
            .file_name
            .as_deref()
            .and_then(extension_of)
            .unwrap_or(DEFAULT_SUBTITLE_EXTENSION)
            .to_string();

        // Facts only the download knows. The search record may lack
        // `extension` (a listing whose name has no subtitle suffix) and never
        // knows the real size.
        let mut fields = BTreeMap::new();
        fields.insert("extension".to_string(), extension.clone());
        if let Some(name) = got.file_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            fields.insert("fileName".to_string(), name.to_string());
        }
        fields.insert("sizeByte".to_string(), got.bytes.len().to_string());

        Ok(vec![HashOutcome {
            hash: Hash(compute_ipfs_cid(&got.bytes)),
            hash_kind: HashKind::Sha2_256,
            bytes: Some(got.bytes.into()),
            record: Some(DiscoveryRecord {
                upstream_id: UPSTREAM_ID.to_string(),
                record_id: format!("{UPSTREAM_ID}:file:{file_id}"),
                fields,
            }),
            file_extension: Some(extension),
        }])
    }

    fn package(&self) -> Option<&'static str> {
        Some(PACKAGE)
    }

    /// Claims `provider-file` for `opensubtitles` whenever an API key is
    /// configured — a login only changes which quota a download spends.
    fn redeems(&self) -> Vec<RedeemClaim> {
        if self.client.is_none() {
            return Vec::new();
        }
        vec![RedeemClaim::provider_file(vec![UPSTREAM_ID.to_string()])]
    }

    fn config_schema(&self) -> ConfigSchema {
        ConfigSchema {
            fields: vec![
                F::secret("apiKey", "OpenSubtitles API key")
                    .required()
                    .with_help(
                        "Consumer API key from opensubtitles.com. Enough for search (which \
                         spends no quota) and for downloads, on the key's anonymous download \
                         quota. Takes effect on the next feeder restart.",
                    ),
                F::text("userAgent", "User-Agent").with_help(
                    "OpenSubtitles expects the registered app name in `App vX.Y` form. \
                     Blank keeps the built-in default.",
                ),
                F::text("username", "Account username (optional)").with_help(
                    "opensubtitles.com account (not .org) to download as, so the account's \
                     rank quota applies instead of the anonymous one. Each download spends \
                     one unit — once per file, since the gateway stores what it fetched. \
                     Blank → downloads on the API key alone.",
                ),
                F::secret("password", "Account password").with_help(
                    "Password for the account above. Takes effect on the next feeder restart.",
                ),
            ],
        }
    }

    fn config_values(&self) -> serde_json::Value {
        serde_json::json!({
            "apiKey": self.config.api_key,
            "userAgent": self.config.user_agent,
            "username": self.config.username,
            "password": self.config.password,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    #[test]
    fn keyword_only() {
        let p = search_params(&q("  matrix ", &[("fileType", "subtitle")])).unwrap();
        assert_eq!(p.query.as_deref(), Some("matrix"));
        assert!(p.imdb_id.is_none() && p.tmdb_id.is_none());
    }

    #[test]
    fn movie_anchor_strips_tt_and_maps_languages() {
        let p = search_params(&q(
            "",
            &[("fileType", "subtitle"), ("imdbid", "tt0133093"), ("languages", "fre,por")],
        ))
        .unwrap();
        assert_eq!(p.imdb_id, Some(133093));
        assert_eq!(p.languages, vec!["fr", "pm", "pt-br", "pt-pt"]);
        assert!(!p.is_episode());
    }

    #[test]
    fn episode_anchor() {
        let p = search_params(&q(
            "",
            &[("fileType", "subtitle"), ("tmdbid", "1399"), ("season", "1"), ("episode", "2")],
        ))
        .unwrap();
        assert_eq!((p.tmdb_id, p.season, p.episode), (Some(1399), Some(1), Some(2)));
        assert!(p.is_episode());
    }

    #[test]
    fn moviehash_must_be_16_hex() {
        let ok = search_params(&q("", &[("moviehash", "8E245D9679D31E12")])).unwrap();
        assert_eq!(ok.moviehash.as_deref(), Some("8e245d9679d31e12"));
        assert!(search_params(&q("", &[("moviehash", "not-a-hash")])).is_none());
    }

    #[test]
    fn nothing_searchable_means_no_call() {
        assert!(search_params(&q("", &[("fileType", "subtitle")])).is_none());
        assert!(search_params(&q("", &[("fileType", "subtitle"), ("languages", "fre")])).is_none());
        // Unknown language: searching without it would only return rows the
        // gateway then drops.
        assert!(search_params(&q("matrix", &[("languages", "zzz")])).is_none());
    }

    #[test]
    fn only_explicit_subtitle_queries_are_answered() {
        assert!(requests_subtitles(&q("", &[("fileType", "subtitle")])));
        assert!(requests_subtitles(&q("", &[("fileType", "video"), ("fileType", "Subtitle")])));
        assert!(!requests_subtitles(&q("matrix", &[])));
        assert!(!requests_subtitles(&q("", &[("fileType", "video")])));
        // Wall / row axes a sidecar never carries.
        for axis in [("domain", "screen"), ("workForm", "standalone"), ("contentKind", "movie")] {
            assert!(!requests_subtitles(&q("matrix", &[("fileType", "subtitle"), axis])), "{axis:?}");
        }
    }
}
