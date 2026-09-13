//! The `provider-file` locator (`0x100A`) — `docs/cid-formats.md` §8.
//!
//! One codec for every provider whose bytes are fetched the same way; the
//! provider lives inside the digest:
//!
//! ```text
//! version   : 1
//! codec     : 0x100A                             (varint 8a 20)
//! multihash : 0x00                               (identity — payload verbatim)
//! digest    : varint(len(source)) ‖ source ‖ id  (both UTF-8, ≤ 64 bytes)
//! ```
//!
//! The encoder/decoder live in `meta-feeder-sdk` (`hash::compute_provider_file_cid`
//! / `decode_provider_file_cid`, with the §8.2 golden vector); this module only
//! binds them to OpenSubtitles' `file:<file_id>` id form.

use crate::consts::UPSTREAM_ID;

pub use meta_feeder_sdk::hash::{
    compute_provider_file_cid as provider_file_cid, decode_provider_file_cid, PROVIDER_FILE_CODEC,
};

/// The locator for one OpenSubtitles **file** (`/download` takes a `file_id`;
/// one subtitle listing can hold several files, one per CD).
pub fn file_cid(file_id: u64) -> Option<String> {
    provider_file_cid(UPSTREAM_ID, &format!("file:{file_id}"))
}

/// The OpenSubtitles `file_id` a `/compute` record id names, if it is ours.
///
/// Accepts both shapes a caller holds: the `provider-file` cid (what the
/// gateway's redeem route sends) and this feeder's own record id
/// `opensubtitles:file:<id>`. A cid for another source is `None` — not ours.
pub fn file_id_of(record_id: &str) -> Option<u64> {
    if let Some(id) = record_id.strip_prefix(&format!("{UPSTREAM_ID}:file:")) {
        return id.parse().ok();
    }
    let (source, id) = decode_provider_file_cid(record_id)?;
    if source != UPSTREAM_ID {
        return None;
    }
    id.strip_prefix("file:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/cid-formats.md` §8.2 worked example — still minted identically now
    /// that the encoder lives in the SDK.
    #[test]
    fn file_cid_matches_the_documented_vector() {
        assert_eq!(
            file_cid(7061834).as_deref(),
            Some("bagfcaaa2bvxxazloon2we5djorwgk43gnfwgkorxga3dcobtgq")
        );
    }

    #[test]
    fn decode_round_trips_and_refuses_other_codecs() {
        let cid = file_cid(12658754).unwrap();
        assert_eq!(
            decode_provider_file_cid(&cid),
            Some(("opensubtitles".to_string(), "file:12658754".to_string()))
        );
        // Identical framing, different codec: a card must not decode as a file.
        assert_eq!(decode_provider_file_cid("bagdsaaanar2g2zdcor3duojvgq3ts"), None);
        assert_eq!(decode_provider_file_cid("not-a-cid"), None);
    }

    #[test]
    fn file_id_of_accepts_both_record_id_shapes() {
        assert_eq!(file_id_of(&file_cid(7061834).unwrap()), Some(7061834));
        assert_eq!(file_id_of("opensubtitles:file:7061834"), Some(7061834));
        // Another provider's file, a card, and garbage are not ours.
        assert_eq!(file_id_of(&provider_file_cid("other", "file:1").unwrap()), None);
        assert_eq!(file_id_of("bagdsaaanar2g2zdcor3duojvgq3ts"), None);
        assert_eq!(file_id_of("opensubtitles:file:abc"), None);
    }
}
