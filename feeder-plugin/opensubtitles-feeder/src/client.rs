//! OpenSubtitles REST v1 client — `GET /subtitles`, plus `POST /login` and
//! `POST /download` for redeeming a file.
//!
//! Search is free. `/download` is **metered per account** (a daily quota), so
//! it runs only from `compute_outcomes`, which the gateway calls on a real play
//! and at most once per file (it stores the bytes). Every API call shares the
//! request-rate [`RateBudget`] (5/s, measured).
//!
//! ⚠ **OpenSubtitles 301s any query string that is not in its canonical form**
//! (keys sorted, values lowercased). Redirects are disabled on the API client, so
//! a drift in [`SearchParams::to_pairs`] surfaces as a loud `Permanent` error
//! instead of a silent extra round-trip per search. The short-lived download
//! *link* is fetched by a second client that does follow redirects.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meta_feeder_sdk::budget::{Lease, RateBudget};
use meta_feeder_sdk::cache::{search_key, MidhashCache};
use meta_feeder_sdk::common::{build_http_client, map_status};
use meta_feeder_sdk::types::GatewayError;
use serde::{Deserialize, Deserializer};
use tokio::sync::Mutex;
use tracing::debug;

use crate::consts::{
    DEFAULT_QUOTA_RETRY_SECS, HTTP_TIMEOUT_SECS, SEARCH_CACHE_TTL_SECS, SEARCH_DEADLINE_SECS,
};

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

// --- download shapes --------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
struct LoginResponse {
    #[serde(default)]
    token: Option<String>,
}

/// `POST /download` answer (also the body of its `406` quota refusal).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DownloadResponse {
    pub link: Option<String>,
    pub file_name: Option<String>,
    #[serde(deserialize_with = "lenient_u64")]
    pub remaining: Option<u64>,
    /// Human text, e.g. `23 hours and 59 minutes`.
    pub reset_time: Option<String>,
    /// e.g. `2022-04-08T13:03:16.000Z`.
    pub reset_time_utc: Option<String>,
    pub message: Option<String>,
}

/// One redeemed subtitle file.
#[derive(Debug, Clone)]
pub struct Downloaded {
    pub bytes: Vec<u8>,
    pub file_name: Option<String>,
    pub remaining: Option<u64>,
}

/// Seconds until the download quota resets: `reset_time_utc` if parseable,
/// else `reset_time` text, else [`DEFAULT_QUOTA_RETRY_SECS`]. Clamped to a day.
pub fn quota_retry_secs(reset_time_utc: Option<&str>, reset_time: Option<&str>, now: u64) -> u32 {
    let secs = reset_time_utc
        .and_then(parse_rfc3339_utc)
        .filter(|at| *at > now)
        .map(|at| at - now)
        .or_else(|| reset_time.and_then(parse_duration_text).filter(|s| *s > 0));
    match secs {
        Some(s) => s.clamp(1, 86_400) as u32,
        None => DEFAULT_QUOTA_RETRY_SECS,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fff](Z|+00:00)` → unix seconds. UTC only — which is
/// all OpenSubtitles sends in `reset_time_utc`.
fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let time = time
        .strip_suffix('Z')
        .or_else(|| time.strip_suffix("+00:00"))?;
    let time = time.split('.').next()?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(|p| p.parse::<i64>());
    let (h, mi, se) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `23 hours and 59 minutes` → seconds. Unknown shapes → `None`.
fn parse_duration_text(s: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut pending: Option<u64> = None;
    let mut matched = false;
    for word in s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        if let Ok(n) = word.parse::<u64>() {
            pending = Some(n);
            continue;
        }
        let Some(n) = pending.take() else { continue };
        let unit = word.to_ascii_lowercase();
        let mult = if unit.starts_with("hour") {
            3600
        } else if unit.starts_with("minute") {
            60
        } else if unit.starts_with("second") {
            1
        } else {
            continue;
        };
        total += n * mult;
        matched = true;
    }
    matched.then_some(total)
}

// --- client -----------------------------------------------------------------

pub struct OsClient {
    http: reqwest::Client,
    /// Fetches the pre-signed download link: redirects allowed (it may bounce
    /// to a CDN), User-Agent only — no Api-Key, no Bearer.
    download_http: reqwest::Client,
    api_base: String,
    api_key: String,
    budget: Arc<RateBudget>,
    cache: Option<MidhashCache>,
    /// Optional account login for `/download`. `None` → downloads run on the
    /// API key alone (OpenSubtitles' anonymous quota); `Some` → a Bearer token
    /// from `/login`, so the account's rank quota applies.
    login: Option<(String, String)>,
    /// Cached JWT from `/login`; dropped and re-fetched once on a 401/403.
    token: Mutex<Option<String>>,
    /// Unix time until which the download quota is known to be spent — no
    /// `/download` call is made before it.
    quota_until: StdMutex<Option<u64>>,
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
        let download_http = build_http_client(HTTP_TIMEOUT_SECS, user_agent, None);
        Self {
            http,
            download_http,
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key,
            budget,
            cache,
            login: None,
            token: Mutex::new(None),
            quota_until: StdMutex::new(None),
        }
    }

    /// Download as an account instead of on the bare API key. Blank credentials
    /// keep the anonymous (API key only) mode.
    pub fn with_login(mut self, username: &str, password: &str) -> Self {
        let (u, p) = (username.trim(), password.trim());
        if !u.is_empty() && !p.is_empty() {
            self.login = Some((u.to_string(), p.to_string()));
        }
        self
    }

    /// True when downloads go through an account login rather than the bare
    /// API key.
    pub fn has_login(&self) -> bool {
        self.login.is_some()
    }

    /// One token from the shared request-rate budget. Busy is Transient.
    async fn api_lease(&self) -> Result<(), GatewayError> {
        if matches!(
            self.budget.acquire(Duration::from_secs(SEARCH_DEADLINE_SECS)).await,
            Lease::DeadlineExceeded
        ) {
            return Err(GatewayError::Transient(
                "opensubtitles: rate budget deadline exceeded".into(),
            ));
        }
        Ok(())
    }

    /// Freeze the whole bucket on a 429, not just this caller.
    fn note_error(&self, e: &GatewayError) {
        if let GatewayError::RateLimited { retry_after_s } = e {
            self.budget
                .note_throttled(Duration::from_secs(u64::from(*retry_after_s)));
        }
    }

    fn quota_retry_after(&self, now: u64) -> Option<u32> {
        let until = (*self.quota_until.lock().expect("quota mutex"))?;
        (until > now).then(|| (until - now).min(u64::from(u32::MAX)) as u32)
    }

    fn block_quota_for(&self, secs: u32, now: u64) {
        *self.quota_until.lock().expect("quota mutex") = Some(now + u64::from(secs));
    }

    async fn login(&self) -> Result<String, GatewayError> {
        let Some((username, password)) = &self.login else {
            return Err(GatewayError::Permanent(
                "opensubtitles: login requested without configured credentials".into(),
            ));
        };
        self.api_lease().await?;
        let resp = self
            .http
            .post(format!("{}/login", self.api_base))
            .header("Api-Key", &self.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({ "username": username, "password": password }))
            .send()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles login: {e}")))?;
        if let Err(e) = map_status(&resp) {
            self.note_error(&e);
            return Err(match e {
                // A missing login endpoint is a broken config, not "not mine".
                GatewayError::NotFound => {
                    GatewayError::Permanent("opensubtitles login: endpoint not found".into())
                }
                other => other,
            });
        }
        let body: LoginResponse = resp
            .json()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles login: bad json: {e}")))?;
        body.token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| GatewayError::Permanent("opensubtitles login returned no token".into()))
    }

    /// The cached token, logging in when there is none (or `refresh` drops it).
    async fn bearer(&self, refresh: bool) -> Result<String, GatewayError> {
        let mut slot = self.token.lock().await;
        if refresh {
            *slot = None;
        }
        if let Some(t) = slot.as_ref() {
            return Ok(t.clone());
        }
        let t = self.login().await?;
        *slot = Some(t.clone());
        Ok(t)
    }

    async fn post_download(&self, token: Option<&str>, file_id: u64) -> Result<reqwest::Response, GatewayError> {
        self.api_lease().await?;
        let mut req = self
            .http
            .post(format!("{}/download", self.api_base))
            .header("Api-Key", &self.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({ "file_id": file_id }));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles download: {e}")))
    }

    /// Redeem one subtitle file: `/download` for a short-lived link, then fetch
    /// it. ⚠ Spends one unit of the daily download quota — the API key's
    /// anonymous quota, or the account's when a login is configured.
    ///
    /// A spent quota (`406`, or `remaining: 0` on the previous call) is
    /// `RateLimited` with the reset time, and blocks further `/download` calls
    /// until then.
    pub async fn download(&self, file_id: u64) -> Result<Downloaded, GatewayError> {
        if let Some(retry_after_s) = self.quota_retry_after(now_secs()) {
            return Err(GatewayError::RateLimited { retry_after_s });
        }

        let resp = if self.has_login() {
            let token = self.bearer(false).await?;
            let resp = self.post_download(Some(&token), file_id).await?;
            if matches!(resp.status().as_u16(), 401 | 403) {
                debug!(target: "opensubtitles", status = %resp.status(), "download refused the token; logging in again");
                let token = self.bearer(true).await?;
                self.post_download(Some(&token), file_id).await?
            } else {
                resp
            }
        } else {
            self.post_download(None, file_id).await?
        };
        if resp.status().as_u16() == 406 {
            let body: DownloadResponse = resp.json().await.unwrap_or_default();
            let now = now_secs();
            let retry = quota_retry_secs(body.reset_time_utc.as_deref(), body.reset_time.as_deref(), now);
            self.block_quota_for(retry, now);
            return Err(GatewayError::RateLimited { retry_after_s: retry });
        }
        if let Err(e) = map_status(&resp) {
            self.note_error(&e);
            return Err(e);
        }
        let body: DownloadResponse = resp
            .json()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles download: bad json: {e}")))?;
        let link = body
            .link
            .clone()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| GatewayError::Transient("opensubtitles download returned no link".into()))?;
        if body.remaining == Some(0) {
            // This file is served; the next one would be refused.
            let now = now_secs();
            let retry = quota_retry_secs(body.reset_time_utc.as_deref(), body.reset_time.as_deref(), now);
            self.block_quota_for(retry, now);
        }

        // The link is pre-signed: keep it out of error messages.
        let got = self
            .download_http
            .get(&link)
            .send()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles file fetch: {}", e.without_url())))?;
        let status = got.status();
        if !status.is_success() {
            return Err(if status.is_server_error() {
                GatewayError::Transient(format!("opensubtitles file fetch: HTTP {status}"))
            } else {
                GatewayError::Permanent(format!("opensubtitles file fetch: HTTP {status}"))
            });
        }
        let bytes = got
            .bytes()
            .await
            .map_err(|e| GatewayError::Transient(format!("opensubtitles file fetch: {}", e.without_url())))?;
        Ok(Downloaded {
            bytes: bytes.to_vec(),
            file_name: body.file_name,
            remaining: body.remaining,
        })
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
    fn quota_reset_prefers_the_utc_timestamp() {
        // 2022-04-08T13:03:16Z = 1649422996.
        assert_eq!(parse_rfc3339_utc("2022-04-08T13:03:16.000Z"), Some(1_649_422_996));
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_utc("yesterday"), None);
        let now = 1_649_422_996 - 90;
        assert_eq!(quota_retry_secs(Some("2022-04-08T13:03:16.000Z"), None, now), 90);
        // Falls back to the text, then to the default.
        assert_eq!(quota_retry_secs(None, Some("23 hours and 59 minutes"), now), 86_340);
        assert_eq!(quota_retry_secs(Some("garbage"), Some("soon"), now), DEFAULT_QUOTA_RETRY_SECS);
        // A reset already in the past is not a retry time.
        assert_eq!(quota_retry_secs(Some("1970-01-01T00:00:00Z"), None, now), DEFAULT_QUOTA_RETRY_SECS);
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
