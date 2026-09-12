//! OpenSubtitles language codes ↔ the stack's ISO 639-2/B vocabulary.
//!
//! ⚠ **The stack compares 639-2/B (`fre`, `ger`, `chi`), not 639-3 (`fra`).**
//! `meta_feeder_sdk::lang::normalize_lang_code` normalises onto it, torznab
//! writes `languages/fre`, and meta-search's language prefs store `fre`.
//! `languages_filter_matches` compares strings, so a record stamped `fra` is
//! *dropped* by a `languages:fre` query — a concrete, non-matching language.
//! (METADATA_KEYS.md still says 639-3 for `languages/*`; the code is what bites.)
//!
//! OpenSubtitles speaks its own dialect: mostly 639-1, plus region tags
//! (`pt-br`, `zh-tw`) and a few private codes (`ze` Chinese bilingual, `sp`/`ea`
//! Spanish EU/LA, `pm` Portuguese MZ). Responses use mixed case (`pt-BR`).

use meta_feeder_sdk::lang::normalize_lang_code;

/// Every `language_code` from `GET /api/v1/infos/languages` (105 entries,
/// fetched 2026-09-12) → ISO 639-2/B. Codes with no 639-2 entry use 639-3
/// (`ast`, `ext`, `cnr`, `tok`), which `normalize_lang_code` passes through.
const OS_TO_639_2B: &[(&str, &str)] = &[
    ("ab", "abk"), ("af", "afr"), ("sq", "alb"), ("am", "amh"), ("ar", "ara"),
    ("an", "arg"), ("hy", "arm"), ("as", "asm"), ("at", "ast"), ("az-az", "aze"),
    ("eu", "baq"), ("be", "bel"), ("bn", "ben"), ("bs", "bos"), ("br", "bre"),
    ("bg", "bul"), ("my", "bur"), ("ca", "cat"), ("ze", "chi"), ("zh-ca", "chi"),
    ("zh-cn", "chi"), ("zh-tw", "chi"), ("hr", "hrv"), ("cs", "cze"), ("da", "dan"),
    ("pr", "per"), ("nl", "dut"), ("en", "eng"), ("eo", "epo"), ("et", "est"),
    ("ex", "ext"), ("fi", "fin"), ("fr", "fre"), ("gd", "gla"), ("gl", "glg"),
    ("ka", "geo"), ("de", "ger"), ("el", "gre"), ("he", "heb"), ("hi", "hin"),
    ("hu", "hun"), ("is", "ice"), ("ig", "ibo"), ("id", "ind"), ("ia", "ina"),
    ("ga", "gle"), ("it", "ita"), ("ja", "jpn"), ("kn", "kan"), ("kk", "kaz"),
    ("km", "khm"), ("ko", "kor"), ("ku", "kur"), ("lv", "lav"), ("lt", "lit"),
    ("lb", "ltz"), ("mk", "mac"), ("ms", "may"), ("ml", "mal"), ("ma", "mni"),
    ("mr", "mar"), ("mn", "mon"), ("me", "cnr"), ("nv", "nav"), ("ne", "nep"),
    ("se", "sme"), ("no", "nor"), ("oc", "oci"), ("or", "ori"), ("fa", "per"),
    ("pl", "pol"), ("pt-pt", "por"), ("pt-br", "por"), ("pm", "por"), ("ps", "pus"),
    ("ro", "rum"), ("ru", "rus"), ("sx", "sat"), ("sr", "srp"), ("sd", "snd"),
    ("si", "sin"), ("sk", "slo"), ("sl", "slv"), ("so", "som"), ("az-zb", "aze"),
    ("es", "spa"), ("sp", "spa"), ("ea", "spa"), ("sw", "swa"), ("sv", "swe"),
    ("sy", "syr"), ("tl", "tgl"), ("ta", "tam"), ("tt", "tat"), ("te", "tel"),
    ("tm-td", "tet"), ("th", "tha"), ("tp", "tok"), ("tr", "tur"), ("tk", "tuk"),
    ("uk", "ukr"), ("ur", "urd"), ("uz", "uzb"), ("vi", "vie"), ("cy", "wel"),
];

/// 639-2/T → 639-2/B for the table's codes that `normalize_lang_code` does not
/// already fold (it covers `fra`/`deu`/`zho`/`nld`/`ell`/`ron`/`slk`/`ces`).
const T_TO_B: &[(&str, &str)] = &[
    ("sqi", "alb"), ("hye", "arm"), ("eus", "baq"), ("mya", "bur"), ("kat", "geo"),
    ("isl", "ice"), ("mkd", "mac"), ("msa", "may"), ("fas", "per"), ("cym", "wel"),
];

/// Canonical 639-2/B for any spelling a query or response might carry: 639-2/B,
/// 639-2/T, 639-1, or an OpenSubtitles code. `None` when nothing maps.
fn canonical(code: &str) -> Option<String> {
    let c = code.trim().to_ascii_lowercase();
    if c.is_empty() {
        return None;
    }
    if let Some((_, b)) = T_TO_B.iter().find(|(t, _)| *t == c) {
        return Some((*b).to_string());
    }
    if let Some((_, b)) = OS_TO_639_2B.iter().find(|(os, _)| *os == c) {
        return Some((*b).to_string());
    }
    let n = normalize_lang_code(&c);
    if n.len() == 3 && n.bytes().all(|b| b.is_ascii_lowercase()) && n != "und" {
        return Some(n);
    }
    // A region-tagged spelling the table does not list (`en-us`): try the
    // primary subtag once.
    let primary = c.split(['-', '_']).next().unwrap_or("");
    if primary != c && !primary.is_empty() {
        return canonical(primary);
    }
    None
}

/// A subtitle's `language` from a search response → the code stamped on the
/// record (`subtitleLanguage`, `languages/<code>`). `None` for unknown input:
/// METADATA_KEYS says `und` is never written — an unknown language is simply
/// absent, which `languages:` filters treat as "may match".
pub fn from_os_code(code: &str) -> Option<String> {
    canonical(code)
}

/// A query's `languages:` value → every OpenSubtitles code that denotes it,
/// sorted (`por` → `pm`, `pt-br`, `pt-pt`). Empty when the language is unknown
/// to OpenSubtitles.
pub fn to_os_codes(lang: &str) -> Vec<&'static str> {
    let Some(b) = canonical(lang) else {
        return Vec::new();
    };
    let mut codes: Vec<&'static str> = OS_TO_639_2B
        .iter()
        .filter(|(_, x)| *x == b)
        .map(|(os, _)| *os)
        .collect();
    codes.sort_unstable();
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_the_full_opensubtitles_list_with_unique_codes() {
        assert_eq!(OS_TO_639_2B.len(), 105);
        let mut os: Vec<_> = OS_TO_639_2B.iter().map(|(o, _)| *o).collect();
        os.sort_unstable();
        os.dedup();
        assert_eq!(os.len(), 105);
    }

    /// Every code the table emits must survive the SDK normaliser unchanged,
    /// or records and `languages:` filters would disagree.
    #[test]
    fn emitted_codes_are_fixed_points_of_the_sdk_normaliser() {
        for (_, b) in OS_TO_639_2B {
            assert_eq!(normalize_lang_code(b), *b, "{b}");
            assert_eq!(canonical(b).as_deref(), Some(*b), "{b}");
        }
    }

    #[test]
    fn response_codes_map_to_639_2b() {
        assert_eq!(from_os_code("fr").as_deref(), Some("fre"));
        assert_eq!(from_os_code("pt-BR").as_deref(), Some("por"));
        assert_eq!(from_os_code("zh-TW").as_deref(), Some("chi"));
        assert_eq!(from_os_code("fa").as_deref(), Some("per"));
        assert_eq!(from_os_code("en-US").as_deref(), Some("eng"));
        assert_eq!(from_os_code(""), None);
        assert_eq!(from_os_code("xx"), None);
        assert_eq!(from_os_code("und"), None);
    }

    #[test]
    fn query_codes_accept_every_spelling() {
        assert_eq!(to_os_codes("fre"), vec!["fr"]);
        assert_eq!(to_os_codes("fra"), vec!["fr"]);
        assert_eq!(to_os_codes("fr"), vec!["fr"]);
        assert_eq!(to_os_codes("FAS"), vec!["fa", "pr"]);
        assert_eq!(to_os_codes("por"), vec!["pm", "pt-br", "pt-pt"]);
        assert_eq!(to_os_codes("spa"), vec!["ea", "es", "sp"]);
        assert!(to_os_codes("zzz").is_empty());
    }
}
