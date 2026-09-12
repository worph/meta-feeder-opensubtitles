# meta-feeder-opensubtitles

Gateway feeder that answers **subtitle searches** from
[OpenSubtitles](https://www.opensubtitles.com). It is a *feeder* (it answers
gateway queries), not a meta-sort *plugin*. The enrichment plugin that attaches
subtitles to local files is the separate `metamesh-plugin-opensubtitle`. See
`docs/project-architecture/plugins-vs-feeders.md` in the meta-root.

**Search only.** Nothing is downloaded, so the account's download quota is never
spent. Each result is one subtitle **file** addressed by a `provider-file`
locator (`0x100A`, `docs/cid-formats.md` §8) that embeds
`("opensubtitles", "file:<file_id>")`. Retrieving the bytes is meta-share's job.

## Queries

Only queries that explicitly ask for `fileType:subtitle` are answered.

| Shape | Example | Upstream call |
|---|---|---|
| keyword | `matrix fileType:subtitle` | `query=matrix` |
| movie anchor | `imdbid:tt0133093 fileType:subtitle` | `imdb_id=133093` |
| episode anchor | `tmdbid:1399 season:1 episode:1 fileType:subtitle` | `parent_tmdb_id=1399&season_number=1&episode_number=1` |
| file hash | `moviehash:8e245d9679d31e12 fileType:subtitle` | `moviehash=…` |

- **Language narrowing:** any shape can add `languages:fre` (ISO 639-2/B; `fra` and `fr` are accepted too).
- **Unanswerable queries** never reach OpenSubtitles: a query with nothing to search on, one with a content kind or domain, or one naming only languages OpenSubtitles doesn't have.

## Records

One record per subtitle file:
- **Type and address:** `fileType=subtitle`, and a `cids/<provider-file cid>` member. The gateway only persists records that carry a cid.
- **Identity:** `opensubtitlesid`, `source/gateway:opensubtitles`.
- **Descriptive:** `title`, `fileName`, `extension` (only when the file name has a real subtitle suffix).
- **Language:** `subtitleLanguage` and `languages/<lang3>`, using ISO 639-2/B codes.
- **Work ids:** `imdbid`, `tmdbid`, `season`, `episode`, `movieYear`. Episodes carry the **show's** ids.
- **Ranking signals:** `downloadCount`, `moviehashMatch`, `hearingImpaired`, `machineTranslated`.

There is no `contentKind` and no `domain`, because a subtitle is a sidecar. Results come back hash matches first, then by download count.

## Configuration

| Setting | Config page key | Env seed |
|---|---|---|
| API key (required) | `apiKey` | `OPENSUBTITLES_API_KEY` |
| User-Agent (`App vX.Y`) | `userAgent` | `OPENSUBTITLES_USER_AGENT` |

The config page (`/config`) wins over env. Changes apply on restart. Without a key
the feeder stays up, reports degraded on `/health`, and answers nothing.

Also `META_FEEDER_HTTP_LISTEN` (default `0.0.0.0:8080`), `META_FEEDER_STATE_DIR`
(default `/data/meta-feeder`), and `RUST_LOG`.

Rate limiting: 4 req/s with a burst of 4. OpenSubtitles serves
`x-ratelimit-limit-second: 5`. Searches are cached for 24 h.

## Build and test

No local cargo is needed. Run from the repo root with a named target volume (a
bind-mounted target is too slow on the WSL mount):

```bash
docker run --rm -e RUSTUP_TOOLCHAIN=1.89.0 \
  -v "$PWD":/build -v osfeeder-target:/build/target \
  -v feeder-cargo-registry:/usr/local/cargo/registry -v osfeeder-cargo-git:/usr/local/cargo/git \
  -w /build rust:1.89-slim-bookworm sh -c \
  'apt-get update -qq && apt-get install -y -qq --no-install-recommends pkg-config libssl-dev ca-certificates git >/dev/null; cargo test'

docker build -f feeder-plugin/opensubtitles-feeder/Dockerfile -t meta-feeder-opensubtitles:dev .
```

`tests/contract.rs` drives the real SDK router against a wiremock OpenSubtitles,
using fixtures captured from the live API.
