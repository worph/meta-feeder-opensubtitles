//! `opensubtitles-feeder` — subtitle search and download over OpenSubtitles.
//!
//! A **sidecar** tier. Search answers "which subtitles exist for X": each result
//! is one subtitle *file*, addressed by a `provider-file` locator (`0x100A`,
//! `docs/cid-formats.md` §8) that embeds `("opensubtitles", "file:<file_id>")`.
//! Searching never spends the account's download quota.
//!
//! Redeeming a locator — `POST /compute` with the cid, which the gateway sends
//! on a real play — logs in and calls `/download`, spending one unit of quota;
//! the gateway stores the bytes so each file is downloaded once.
//!
//! ## The contracts that must not drift
//!
//! - The locator bytes ([`locator`]), pinned by the golden vector in
//!   `cid-formats.md` §8.2.
//! - Every record carries a `cids/` member. The gateway persists only records
//!   with a cid, and its search-coverage skip is only sound for persisted records.
//! - Language codes are ISO 639-2/B (`fre`, not `fra`) — the vocabulary
//!   `meta_feeder_sdk::lang::normalize_lang_code` and every `languages:` filter use.

pub mod client;
pub mod consts;
pub mod lang;
pub mod locator;
pub mod plugin;
pub mod record;
