//! `OpenSubtitlesPlugin` — the `FeederPlugin` for `fileType:subtitle`.
//!
//! Two query shapes, both answered; which one to send is the client's call:
//!
//! - **keyword** — `matrix fileType:subtitle` → `query=matrix`
//! - **anchored** — any of `imdbid:` `tmdbid:` `season:` `episode:`
//!   `movieYear:` `moviehash:`, e.g. `tmdbid:1399 season:1 episode:1 fileType:subtitle`
//!
//! plus an optional `languages:` narrowing on either.

use std::path::Path;

use async_trait::async_trait;
use meta_feeder_sdk::budget::RateBudget;
use meta_feeder_sdk::common::open_midhash_cache;
use meta_feeder_sdk::config::{ConfigField as F, ConfigSchema};
use meta_feeder_sdk::plugin::{ConfigError, FeederPlugin, HashOutcome};
use meta_feeder_sdk::query::GatewayQuery;
use meta_feeder_sdk::query_eval::query_accepts_plugin;
use meta_feeder_sdk::types::{DiscoveryRecord, GatewayError, PluginHealth};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::client::{OsClient, SearchParams};
use crate::consts::{
    DEFAULT_USER_AGENT, ENV_API_KEY, ENV_USER_AGENT, OS_API_BASE, OS_BURST, OS_RATE_PER_SEC,
    UPSTREAM_ID,
};
use crate::lang::to_os_codes;
use crate::record::project;

/// Keys match [`FeederPlugin::config_schema`], which is what the SDK writes to
/// `config.json` from the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OsConfig {
    #[serde(default, rename = "apiKey")]
    api_key: String,
    #[serde(default, rename = "userAgent")]
    user_agent: String,
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
                user_agent: String::new(),
            },
            api_base,
            client: None,
        }
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
        self.client = Some(OsClient::new(
            self.api_base.clone(),
            api_key,
            user_agent,
            RateBudget::new(OS_RATE_PER_SEC, OS_BURST),
            Some(cache),
        ));
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

    /// Not implemented, on purpose: a `provider-file` locator is resolved by
    /// meta-share's fetch tier (`docs/cid-formats.md` §8), and the SDK's
    /// `HashKind` has no variant to carry one — adding it is an SDK + gateway
    /// change. Searching never needs this path.
    async fn compute_outcomes(&self, _record_id: &str) -> Result<Vec<HashOutcome>, GatewayError> {
        Err(GatewayError::Permanent(
            "opensubtitles: provider-file locators are resolved by meta-share, not /compute \
             (docs/cid-formats.md §8)"
                .into(),
        ))
    }

    fn config_schema(&self) -> ConfigSchema {
        ConfigSchema {
            fields: vec![
                F::secret("apiKey", "OpenSubtitles API key")
                    .required()
                    .with_help(
                        "Consumer API key from opensubtitles.com. Search only — this feeder \
                         never downloads, so no account login is needed and no download \
                         quota is spent. Takes effect on the next feeder restart.",
                    ),
                F::text("userAgent", "User-Agent").with_help(
                    "OpenSubtitles expects the registered app name in `App vX.Y` form. \
                     Blank keeps the built-in default.",
                ),
            ],
        }
    }

    fn config_values(&self) -> serde_json::Value {
        serde_json::json!({
            "apiKey": self.config.api_key,
            "userAgent": self.config.user_agent,
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
