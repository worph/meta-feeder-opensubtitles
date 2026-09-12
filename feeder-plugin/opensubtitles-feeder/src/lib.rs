//! `opensubtitles-feeder` — subtitle search over OpenSubtitles.
//!
//! A **sidecar** tier: it answers "which subtitles exist for X", never "give me
//! the bytes". Each result is one subtitle *file*, addressed by a
//! `provider-file` locator (`0x100A`, `docs/cid-formats.md` §8) that embeds
//! `("opensubtitles", "file:<file_id>")`. meta-share resolves that locator later;
//! nothing here downloads, so searching never spends the account's download quota.
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
