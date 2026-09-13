//! Tunables, each with the reason for its value.

/// OpenSubtitles REST API v1 base.
pub(crate) const OS_API_BASE: &str = "https://api.opensubtitles.com/api/v1";

/// `upstream_id` of the one plugin this feeder hosts, and the `source` token
/// inside every `provider-file` locator it mints.
pub const UPSTREAM_ID: &str = "opensubtitles";

/// Provenance label (`source/<label>`, METADATA_KEYS §5).
pub(crate) const SOURCE_LABEL: &str = "gateway:opensubtitles";

/// OpenSubtitles rejects anonymous clients: it wants a named app in `App vX.Y`
/// form. Overridable, because the key's registered app name is what they check.
pub(crate) const DEFAULT_USER_AGENT: &str =
    concat!("meta-feeder-opensubtitles v", env!("CARGO_PKG_VERSION"));

/// ⚠ **5 requests per second, measured, not guessed.** A live search returns
/// `x-ratelimit-limit-second: 5` (2026-09-12). 4/s with a burst of 4 stays under
/// it with room for another client sharing the key.
pub(crate) const OS_RATE_PER_SEC: f64 = 4.0;
pub(crate) const OS_BURST: f64 = 4.0;

/// How long a search waits for a rate-budget lease. Someone is watching a
/// spinner, so it may queue, but not past the gateway's own patience.
pub(crate) const SEARCH_DEADLINE_SECS: u64 = 20;

/// HTTP timeout for a single upstream call.
pub(crate) const HTTP_TIMEOUT_SECS: u64 = 20;

/// Search results are cached, but not forever: a listing keeps gaining uploads
/// and download counts, and download count is half of the ranking.
pub(crate) const SEARCH_CACHE_TTL_SECS: u64 = 24 * 60 * 60;

/// `/files/plugin/<PACKAGE>/` — where the gateway stores downloaded subtitles.
pub const PACKAGE: &str = "meta-feeder-opensubtitles";

/// `retry_after_s` when the download quota is spent and OpenSubtitles gave no
/// parseable reset time. The quota is daily.
pub(crate) const DEFAULT_QUOTA_RETRY_SECS: u32 = 3600;

/// A "subtitle" shorter than this is an error page or an empty file, not a
/// subtitle — refuse it rather than seed it to the mesh.
pub(crate) const MIN_SUBTITLE_BYTES: usize = 10;

/// Extension used when the downloaded file name has no known subtitle suffix.
pub(crate) const DEFAULT_SUBTITLE_EXTENSION: &str = "srt";

/// Env seeds; the config page's saved values win over them.
pub(crate) const ENV_API_KEY: &str = "OPENSUBTITLES_API_KEY";
pub(crate) const ENV_USER_AGENT: &str = "OPENSUBTITLES_USER_AGENT";
pub(crate) const ENV_USERNAME: &str = "OPENSUBTITLES_USERNAME";
pub(crate) const ENV_PASSWORD: &str = "OPENSUBTITLES_PASSWORD";
