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
//! The framing is byte-identical to `card` (`0x1007`) — only the codec differs —
//! which is why the tests below re-derive the existing `card-locator` vector
//! with the same encoder.
//!
//! ⚠ **This is a local copy.** The SDK's framing helpers (`kinded_locator_cid`,
//! `write_pb_varint`, `base32_lower_no_padding`) are private, so it could not be
//! reused. Move it into `meta-feeder-sdk` as `encode_source_id_locator(codec,
//! source, id)` (§8 trap 7) and delete this file; the golden vectors are what
//! prove the move changed no bytes.

use crate::consts::UPSTREAM_ID;

/// Custom multicodec for a fetchable `(source, id)` locator.
pub const PROVIDER_FILE_CODEC: u64 = 0x100A;

/// meta-share's `MAX_MULTIHASH_SIZE` — its `CidGeneric<64>` rejects a longer
/// multihash, so a longer digest would mint a CID no peer can parse.
const MAX_DIGEST_BYTES: usize = 64;

/// The locator for one OpenSubtitles **file** (`/download` takes a `file_id`;
/// one subtitle listing can hold several files, one per CD).
pub fn file_cid(file_id: u64) -> Option<String> {
    provider_file_cid(UPSTREAM_ID, &format!("file:{file_id}"))
}

/// Encode a `provider-file` CID. `None` when the digest would exceed 64 bytes —
/// the caller drops that row rather than emit an unparseable address.
pub fn provider_file_cid(source: &str, id: &str) -> Option<String> {
    source_id_locator(PROVIDER_FILE_CODEC, source, id)
}

/// Inverse of [`provider_file_cid`]: `(source, id)`, or `None` for anything that
/// is not exactly a well-formed `0x100A` CID. Matches the **codec slot** — a
/// `card` CID has the same framing and must not decode here.
pub fn decode_provider_file_cid(cid: &str) -> Option<(String, String)> {
    let wire = base32_lower_no_pad_decode(cid.strip_prefix('b')?)?;
    let mut pos = 0;
    if read_varint(&wire, &mut pos)? != 1 {
        return None;
    }
    if read_varint(&wire, &mut pos)? != PROVIDER_FILE_CODEC {
        return None;
    }
    if read_varint(&wire, &mut pos)? != 0x00 {
        return None;
    }
    let len = read_varint(&wire, &mut pos)? as usize;
    let digest = wire.get(pos..)?;
    if digest.len() != len {
        return None;
    }
    let mut dpos = 0;
    let source_len = read_varint(digest, &mut dpos)? as usize;
    let source = digest.get(dpos..dpos.checked_add(source_len)?)?;
    let id = digest.get(dpos + source_len..)?;
    Some((
        String::from_utf8(source.to_vec()).ok()?,
        String::from_utf8(id.to_vec()).ok()?,
    ))
}

fn source_id_locator(codec: u64, source: &str, id: &str) -> Option<String> {
    let source = source.as_bytes();
    let id = id.as_bytes();

    // identity-multihash digest = varint(source_len) ‖ source ‖ id
    let mut digest = Vec::with_capacity(2 + source.len() + id.len());
    write_varint(source.len() as u64, &mut digest);
    digest.extend_from_slice(source);
    digest.extend_from_slice(id);
    if digest.len() > MAX_DIGEST_BYTES {
        return None;
    }

    // CIDv1: [version=0x01][codec varint][mh code=0x00 identity][len varint][digest]
    let mut wire = Vec::with_capacity(6 + digest.len());
    wire.push(0x01);
    write_varint(codec, &mut wire);
    wire.push(0x00);
    write_varint(digest.len() as u64, &mut wire);
    wire.extend_from_slice(&digest);
    Some(format!("b{}", base32_lower_no_pad(&wire)))
}

fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

const BASE32_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn base32_lower_no_pad(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() * 8).div_ceil(5));
    let mut acc = 0u64;
    let mut bits = 0u32;
    for &byte in input {
        acc = (acc << 8) | u64::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1u64 << bits) - 1;
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_lower_no_pad_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 5 / 8);
    let mut acc = 0u64;
    let mut bits = 0u32;
    for c in input.bytes() {
        let v = match c {
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5) | u64::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1u64 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same framing under `card`'s codec reproduces the existing
    /// `card-locator` golden vector — proof the encoder is the family's framing,
    /// not a look-alike.
    #[test]
    fn framing_reproduces_the_card_locator_vector() {
        assert_eq!(
            source_id_locator(0x1007, "tmdb", "tv:95479").as_deref(),
            Some("bagdsaaanar2g2zdcor3duojvgq3ts")
        );
        assert_eq!(
            source_id_locator(0x1008, "video", "4D7u5KF7SP8").as_deref(),
            Some("bagecaaarav3gszdfn42ein3vgvfumn2tka4a")
        );
    }

    /// `docs/cid-formats.md` §8.2 worked example.
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
        assert_eq!(decode_provider_file_cid(""), None);
    }

    #[test]
    fn digest_ceiling_is_64_bytes() {
        // varint(13) + "opensubtitles" = 14 bytes, so the id may use 50.
        assert!(provider_file_cid("opensubtitles", &"x".repeat(50)).is_some());
        assert!(provider_file_cid("opensubtitles", &"x".repeat(51)).is_none());
    }
}
