# meta-feeder-opensubtitles

Gateway feeder that answers **subtitle searches** from
[OpenSubtitles](https://www.opensubtitles.com). It is a *feeder* (it answers
gateway queries), not a meta-sort *plugin*. The enrichment plugin that attaches
subtitles to local files is the separate `metamesh-plugin-opensubtitle`. See
`docs/project-architecture/plugins-vs-feeders.md` in the meta-root.

**Search is free.** Each result is one subtitle **file** addressed by a
`provider-file` locator (`0x100A`, `docs/cid-formats.md` §8) that embeds
`("opensubtitles", "file:<file_id>")`; searching never spends download quota.

**Downloads are redeems.** When a viewer plays a subtitle, the gateway asks this
feeder to redeem the locator (`POST /compute`), which calls OpenSubtitles
`/download` — one unit of the daily quota: the API key's anonymous quota, or an
account's rank quota when a login is configured. The gateway stores the file, so
each file is downloaded once. See [Downloads](#downloads).

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
| Account username (optional, opensubtitles.com) | `username` | `OPENSUBTITLES_USERNAME` |
| Account password (optional) | `password` | `OPENSUBTITLES_PASSWORD` |

The config page (`/config`) wins over env. Changes apply on restart. Without a key
the feeder stays up, reports degraded on `/health`, and answers nothing. Without a
login it still downloads, on the API key's anonymous quota; a login (a
**.com** account — .org accounts are a different API) switches to that account's
rank quota.

## Downloads

`POST /compute {upstream_id: "opensubtitles", record_id}` where `record_id` is the
`provider-file` cid (or `opensubtitles:file:<id>`):

1. Not an `opensubtitles` file locator → `404` (not ours).
2. `POST /download {file_id}` with the `Api-Key` header → a short-lived link,
   fetched by a second client that follows redirects and sends only the
   User-Agent. With a login configured the call also carries a Bearer token from
   `POST /login` (cached; re-login once on a `401`/`403`).
3. Answer ONE `sha2_256` outcome: the bytes, `file_extension` from the file name
   (`srt` when unknown), and a record holding only what the download knows —
   `extension`, `fileName`, `sizeByte`. The gateway merges those onto the search
   record (which already has title, language and ids) next to the `file` pointer.

Quota: a `406`, or `remaining: 0` on the previous download, answers `429` with the
reset time (`reset_time_utc`, else `reset_time`, else 1 h) and makes no further
`/download` call until then. A file under 10 bytes → `422`.

`GET /redeems` and `/manifest` advertise `{codec: provider-file, field: file,
sources: ["opensubtitles"]}` whenever an API key is configured; `package` is
`meta-feeder-opensubtitles` (`/files/plugin/meta-feeder-opensubtitles/`).

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

The SDK is pinned by tag (`v1.3.0`). Until that tag is pushed, cargo cannot
resolve it — not even under a `[patch]` override — so the recipe above fails. To
verify against a local SDK checkout, run it on a scratch copy with the
dependency swapped for a path (never commit that form):

```bash
SCRATCH=/d/workspace/tmp-claude/os-feeder; SDK=/d/workspace/MetaMesh/meta-root-v2/packages/plugins/meta-feeder-sdk
rm -rf $SCRATCH && mkdir -p $SCRATCH && tar cf - --exclude=./target --exclude=./.git . | (cd $SCRATCH && tar xf -)
sed -i "s#meta-feeder-sdk = { git = .*#meta-feeder-sdk = { path = \"$SDK\" }#" $SCRATCH/feeder-plugin/*/Cargo.toml
# then the docker `cargo test` above with -v /d/workspace:/d/workspace -w $SCRATCH
```

After the tag is pushed, run `cargo update -p meta-feeder-sdk` to move
`Cargo.lock` off the v1.2.1 revision.
