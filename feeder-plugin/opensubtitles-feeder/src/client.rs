//! OpenSubtitles REST v1 search client — `GET /subtitles` only.
//!
//! Search is free: OpenSubtitles meters `/download`, which this feeder never
//! calls. What *is* limited is the request rate (5/s, measured), hence the
//! shared [`RateBudget`].
//!
//! ⚠ **OpenSubtitles 301s any query string that is not in its canonical form**
//! (keys sorted, values lowercased). Redirects are disabled on this client, so
//! a drift in [`SearchParams::to_pairs`] surfaces as a loud `Permanent` error
//! instead of a silent extra round-trip per search.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meta_feeder_sdk::budget::{Lease, RateBudget};
use meta_feeder_sdk::cache::{search_key, MidhashCache};
use meta_feeder_sdk::common::{build_http_client, map_status};
use meta_feeder_sdk::types::GatewayError;
use serde::{Deserialize, Deserializer};
use tracing::debug;

use crate::consts::{HTTP_TIMEOUT_SECS, SEARCH_CACHE_TTL_SECS, SEARCH_DEADLINE_SECS};

/// One search, already resolved from a gateway query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchParams {
    /// Free text (`query=`).
    pub query: Option<String>,
    /// IMDb id, numeric (the `tt` prefix stripped).
    pub imdb_id: Option<u64>,
    pub tmdb_id: Option<u64>,
    pub season: Option<u64>,
    pub episode: Option<u64>,
    pub year: Option<u64>,
    /// 16-hex OpenSubtitles moviehash, lowercase.
    pub moviehash: Option<String>,
    /// OpenSubtitles language codes (`fr`, `pt-br`).
    pub languages: Vec<&'static str>,
}

impl SearchParams {
    /// A season or episode makes the ids **show** ids, sent as `parent_*`.
    /// (A live probe returns identical results for `tmdb_id` and
    /// `parent_tmdb_id` on a show, but `parent_*` is the documented form.)
    pub fn is_episode(&self) -> bool {
        self.season.is_some() || self.episode.is_some()
    }

    /// Nothing to search on. Never send this: OpenSubtitles would answer with
    /// an arbitrary listing.
    pub fn is_unbounded(&self) -> bool {
        self.query.is_none()
            && self.imdb_id.is_none()
            && self.tmdb_id.is_none()
            && self.moviehash.is_none()
    }

    /// Query parameters in OpenSubtitles' canonical form: keys sorted, values
    /// lowercased, language list sorted and de-duplicated.
    pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let episode = self.is_episode();
        let mut pairs: Vec<(&'static str, String)> = Vec::new();
        if let Some(e) = self.episode {
            pairs.push(("episode_number", e.to_string()));
        }
        if let Some(id) = self.imdb_id {
            pairs.push((if episode { "parent_imdb_id" } else { "imdb_id" }, id.to_string()));
        }
        if !self.languages.is_empty() {
            let mut langs = self.languages.clone();
            langs.sort_unstable();
            langs.dedup();
            pairs.push(("languages", langs.join(",")));
        }
        if let Some(h) = &self.moviehash {
            pairs.push(("moviehash", h.to_ascii_lowercase()));
        }
        if let Some(id) = self.tmdb_id {
            pairs.push((if episode { "parent_tmdb_id" } else { "tmdb_id" }, id.to_string()));
        }
        if let Some(q) = &self.query {
            pairs.push(("query", q.to_lowercase()));
        }
        if let Some(s) = self.season {
            pairs.push(("season_number", s.to_string()));
        }
        if let Some(y) = self.year {
            pairs.push(("year", y.to_string()));
        }
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        pairs
    }
}

/// `k=v&…` with values percent-encoded, except `,` (the language list
/// separator) and space as `+`. Built by hand rather than with
/// `RequestBuilder::query` so the exact bytes OpenSubtitles canonicalises
/// against are under test.
pub fn encode_query(pairs: &[(&'static str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_value(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b',' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// --- response shape ---------------------------------------------------------
//
// Every field is optional and numbers are parsed leniently: one odd value in
// one row must not fail the whole page.

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub data: Vec<OsSubtitle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OsSubtitle {
    #[serde(default)]
    pub attributes: OsAttributes,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OsAttributes {
    pub language: Option<String>,
    pub release: Option<String>,
    #[serde(deserialize_with = "lenient_u64")]
    pub download_count: Option<u64>,
    /// Present only on a `moviehash` search.
    pub moviehash_match: Option<bool>,
    pub hearing_impaired: Option<bool>,
    pub ai_translated: Option<bool>,
    pub machine_translated: Option<bool>,
    #[serde(deserialize_with = "lenient_u64")]
    pub nb_cd: Option<u64>,
    pub feature_details: Option<FeatureDetails>,
    pub files: Vec<OsFile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FeatureDetails {
    /// `Movie` | `Episode` | `Tvshow`.
    pub feature_type: Option<String>,
    #[serde(deserialize_with = "lenient_u64")]
    pub year: Option<u64>,
    pub title: Option<String>,
    /// Display name, e.g. `Game of Thrones - S01E01  Winter Is Coming`.
    pub movie_name: Option<String>,
    #[serde(deserialize_with = "lenient_u64")]
    pub imdb_id: Option<u64>,
    #[serde(deserialize_with = "lenient_u64")]
    pub tmdb_id: Option<u64>,
    #[serde(deserialize_with = "lenient_u64")]
    pub season_number: Option<u64>,
    #[serde(deserialize_with = "lenient_u64")]
    pub episode_number: Option<u64>,
    #[serde(deserialize_with = "lenient_u64")]
    pub parent_imdb_id: Option<u64>,
    #[serde(deserialize_with = "lenient_u64")]
    pub parent_tmdb_id: Option<u64>,
    pub parent_title: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OsFile {
    #[serde(deserialize_with = "lenient_u64")]
    pub file_id: Option<u64>,
    #[serde(deserialize_with = "lenient_u64")]
    pub cd_number: Option<u64>,
    pub file_name: Option<String>,
}

fn lenient_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                .map(|f| f as u64)
        }),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

// --- cache ------------------------------------------------------------------

fn cache_key(pairs: &[(&'static str, String)]) -> String {
    search_key("os-subtitles", &encode_query(pairs))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_entry(body: &str, fetched_at: u64) -> String {
    serde_json::json!({ "fetchedAt": fetched_at, "body": body }).to_string()
}

/// The cached body if the entry is younger than `ttl_secs`.
fn fresh_body(entry: &str, now: u64, ttl_secs: u64) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(entry).ok()?;
    let fetched_at = v.get("fetchedAt")?.as_u64()?;
    if now.saturating_sub(fetched_at) > ttl_secs {
        return None;
    }
    v.get("body")?.as_str().map(str::to_string)
}

// --- client -----------------------------------------------------------------

pub struct OsClient {
    http: reqwest::Client,
    api_base: String,
    api_key: String,
    budget: Arc<RateBudget>,
    cache: Option<MidhashCache>,
}

impl OsClient {
    pub fn new(
        api_base: String,
        api_key: String,
        user_agent: &str,
        budget: Arc<RateBudget>,
        cache: Option<MidhashCache>,
    ) -> Self {
        let http = build_http_client(
            HTTP_TIMEOUT_SECS,
            user_agent,
            Some(reqwest::redirect::Policy::none()),
        );
        Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key,
            budget,
            cache,
        }
    }

    pub async fn search(&self, params: &SearchParams) -> Result<Vec<OsSubtitle>, GatewayError> {
        let pairs = params.to_pairs();
        let key = cache_key(&pairs);

        if let Some(cache) = &self.cache {
            if let Ok(Some(entry)) = cache.get_misc(&key) {
                if let Some(body) = fresh_body(&entry, now_secs(), SEARCH_CACHE_TTL_SECS) {
                    if let Ok(parsed) = serde_json::from_str::<SearchResponse>(&body) {
                        debug!(target: "opensubtitles", %key, "search cache hit");
                        return Ok(parsed.data);
                    }
                }
            }
        }

        if matches!(
            self.budget.acquire(Duration::from_secs(SEARCH_DEADLINE_SECS)).await,
            Lease::DeadlineExceeded
        ) {
            // Transient, not Permanent: busy is not broken.
            return Err(GatewayError::Transient(
                "opensubtitles: rate budget deadline exceeded".into(),
            ));
        }

        let url = format!("{}/subtitles?{}", self.api_base, encode_query(&pairs));
        let resp = self
            .http
            .get(&url)
            .header("Api-Key", &self.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles: {e}")))?;

        // 429 carries Retry-After; freeze the whole bucket, not just this caller.
        // A 301 (non-canonical query) lands here as Permanent — see the module doc.
        if let Err(e) = map_status(&resp) {
            if let GatewayError::RateLimited { retry_after_s } = &e {
                self.budget
                    .note_throttled(Duration::from_secs(u64::from(*retry_after_s)));
            }
            return Err(e);
        }

        let body = resp
            .text()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles: {e}")))?;
        let parsed: SearchResponse = serde_json::from_str(&body)
            .map_err(|e| GatewayError::Transient(format!("opensubtitles: bad json: {e}")))?;

        if let Some(cache) = &self.cache {
            let _ = cache.put_misc(&key, &cache_entry(&body, now_secs()));
        }
        Ok(parsed.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_are_sorted_and_lowercased() {
        let p = SearchParams {
            query: Some("The Matrix".into()),
            imdb_id: Some(133093),
            year: Some(1999),
            languages: vec!["pt-br", "fr", "fr"],
            moviehash: Some("8E245D9679D31E12".into()),
            ..Default::default()
        };
        assert_eq!(
            encode_query(&p.to_pairs()),
            "imdb_id=133093&languages=fr,pt-br&moviehash=8e245d9679d31e12&query=the+matrix&year=1999"
        );
    }

    #[test]
    fn episode_ids_are_sent_as_parent_ids() {
        let p = SearchParams {
            tmdb_id: Some(1399),
            imdb_id: Some(944947),
            season: Some(1),
            episode: Some(1),
            ..Default::default()
        };
        assert_eq!(
            encode_query(&p.to_pairs()),
            "episode_number=1&parent_imdb_id=944947&parent_tmdb_id=1399&season_number=1"
        );
    }

    #[test]
    fn unbounded_means_no_text_no_id_no_hash() {
        assert!(SearchParams::default().is_unbounded());
        let langs_only = SearchParams { languages: vec!["fr"], ..Default::default() };
        assert!(langs_only.is_unbounded());
        let text = SearchParams { query: Some("matrix".into()), ..Default::default() };
        assert!(!text.is_unbounded());
    }

    #[test]
    fn value_encoding_escapes_everything_but_unreserved_and_comma() {
        assert_eq!(encode_value("léon: the professional"), "l%C3%A9on%3A+the+professional");
    }

    #[test]
    fn cache_entries_expire() {
        let entry = cache_entry("{\"data\":[]}", 1_000);
        assert_eq!(fresh_body(&entry, 1_000 + 10, 60).as_deref(), Some("{\"data\":[]}"));
        assert_eq!(fresh_body(&entry, 1_000 + 61, 60), None);
        assert_eq!(fresh_body("garbage", 0, 60), None);
    }

    #[test]
    fn lenient_numbers_survive_odd_upstream_values() {
        let a: OsAttributes = serde_json::from_str(
            r#"{"download_count":"42","nb_cd":null,"files":[{"file_id":7.0,"cd_number":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(a.download_count, Some(42));
        assert_eq!(a.nb_cd, None);
        assert_eq!(a.files[0].file_id, Some(7));
        assert_eq!(a.files[0].cd_number, None);
    }
}
