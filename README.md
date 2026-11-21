# teapawt

A privacy-focused Twitter/X frontend written in Rust.

Public instances: [nitter.cf](https://nitter.cf), [nitter.gq](https://nitter.gq), [xitter.cf](https://xitter.cf), [xitter.gq](https://xitter.gq), [teapaw.org](https://teapaw.org), [teapawt.net](https://teapawt.net), and [teapawt.org](https://teapawt.org).

Live status for those instances: [status.d420.net](https://status.d420.net). Scraping is allowed on them. Ask the operator before scraping any other Nitter instance.

## Features

- **Privacy-focused**: No third-party JavaScript (unless optional Cloudflare Turnstile gates are configured), tracking, ads, or remote fonts
- **RSS feeds**: Subscribe to user timelines, threads, lists, and search results
- **Browser-impersonating HTTP/3 & HTTP/2 client (`koon-core`)**: Each account deterministically presents a real desktop browser's TLS, HTTP/2, and HTTP/3 (QUIC) fingerprint (Chrome, Edge, Firefox, Safari, or Opera), including HTTP/3 over SOCKS5 `UDP ASSOCIATE` with automatic TCP/HTTP/2 fallback
- **Separate `search` and `view` account pools**: Search accounts stay isolated from profile and tweet reads, with per-account cookie jars, background NSFW/search-safety filter sync, search auditing, and live imports without restarts
- **SOCKS5 exit pool**: Stable session-to-port pinning, per-port edge-block circuit breaking, and optional fallback exits for HTML 429s
- **PostgreSQL-backed state**: Account inventory, cookie jars, per-account rate limits, sharded screen-name-to-numeric-id directory, plus optional operational telemetry (`/admin/stats`) and paid search passes (`/search-pass`). HTML pages themselves are not cached in the process
- **FxEmbed-style Discord embeds**:
  - Multiple images displayed in carousel on mobile
  - Videos play directly in the embed
  - Optional GIF transcoding (`ffmpeg` locally or an external transcoding domain)
  - Proper image dimensions for better layout
  - ActivityPub JSON endpoint for rich embeds

## Requirements

- **Rust 1.96.1+** (edition 2024), plus a C/C++ toolchain, `cmake`, and `clang`/`libclang` (required to compile BoringSSL via `koon-core` and `tikv-jemallocator`)
- **PostgreSQL** (**required** — stores accounts, cookie jars, and rate limits; `teapot` will refuse to start if PostgreSQL cannot be reached or before the initial account migration has been run)
- **Twitter/X session tokens** for API access (browser cookies `auth_token` + `ct0`, or OAuth tokens)
- **Optional**: `ffmpeg` on `PATH` for local GIF transcoding (`gifTranscoding.mode = "local"`, the default; falls back to `"off"` if missing)
- **Optional**: SOCKS5 or HTTP proxy exits if X blocks or throttles the server's own IP

## Building

```bash
cargo build --release
```

The crate and main server binary stay named `teapot` so existing configs and process managers keep working. The build produces three binaries in `target/release/`:

- `teapot` — the web server
- `import-accounts` — offline PostgreSQL account migration and import CLI
- `scrape-socks` — optional local SOCKS5 (TCP + `UDP ASSOCIATE`) forwarder for WireGuard/Mullvad exits

## Configuration and Setup

### 1. Create `config/teapot.toml`

```bash
cp config/teapot.example.toml config/teapot.toml
```

Edit `config/teapot.toml`:

- **Set `config.hmacKey`** to a unique secret of at least 32 characters (e.g. `openssl rand -hex 32`). Startup refuses empty, default, or shorter keys.
- Set `server.title` (e.g. `"teapawt"` or whatever you want shown in the navbar).
- Behind nginx or another reverse proxy / TLS terminator, set `https = true`, `publicPort = 443`, and `config.trustedProxies = ["127.0.0.1"]` (or your proxy's IP) so generated links omit the listen port and per-visitor budgets read `X-Forwarded-For` correctly.
- If `ffmpeg` is not installed and you do not want local GIF transcoding, set `mode = "off"` under `[gifTranscoding]` (otherwise startup logs an error and falls back to `"off"`).

### 2. Set up PostgreSQL

PostgreSQL is required for accounts, cookie jars, and rate limits. By default both `teapot` and `import-accounts` connect via peer auth on the local Unix socket:

```
host=/var/run/postgresql user=teapot dbname=teapot
```

Create the role and database:

```sql
CREATE ROLE teapot LOGIN;
CREATE DATABASE teapot OWNER teapot;
```

If you run `teapot` as an OS user other than `teapot`, either map that user in PostgreSQL's `pg_ident.conf`, or override the connection string with `TEAPOT_LIMITS_DATABASE_URL` (standard libpq key=value or `postgres://` URI, without TLS):

```bash
# Example using TCP + password auth
export TEAPOT_LIMITS_DATABASE_URL="postgres://teapot:your-password@127.0.0.1/teapot"

# Or using local peer auth as your current OS user (after `createdb teapot`)
export TEAPOT_LIMITS_DATABASE_URL="host=/var/run/postgresql user=$USER dbname=teapot"
```

All tables are created automatically on connect. See [PostgreSQL](#postgresql) below for the full schema breakdown.

### 3. Seed and migrate accounts into PostgreSQL

`teapot` **never reads `sessions.jsonl` at runtime** — accounts live in PostgreSQL, and the server refuses to start until the one-time offline migration has completed.

1. Create `sessions.jsonl` with your initial Twitter/X credentials (or create an empty file if you plan to import accounts with `import-accounts import` in step 3):

```bash
cp sessions.example.jsonl sessions.jsonl
# Edit sessions.jsonl with your auth_token and ct0 from browser cookies
```

Cookie sessions use `kind = "cookie"` with `auth_token` and `ct0`; OAuth sessions use `kind = "oauth"` with `oauth_token` and `oauth_secret` (give each line in `sessions.jsonl` a unique numeric `id`). Add `"pool": "search"` to any line that should belong to the dedicated search pool; omitting `"pool"` puts the account in the view pool.

2. Run the one-time migration (with `TEAPOT_LIMITS_DATABASE_URL` set if you are not using the default socket/role):

```bash
./target/release/import-accounts migrate
```

This reads `TEAPOT_SESSIONS_FILE` (default: `sessions.jsonl`) and optional `TEAPOT_COOKIES_FILE` (default: `session-cookies.json`), loads the accounts and cookie jars into PostgreSQL, and records the `legacy-v1` migration marker so `teapot` can start.

3. To add more accounts later without editing `sessions.jsonl`:

```bash
# Import into the view pool (profiles, tweets, threads, lists)
./target/release/import-accounts import view-accounts.txt view

# Import into the dedicated search pool
./target/release/import-accounts import search-accounts.txt search

# Show total / search / view account counts
./target/release/import-accounts stats

# Restore a previously demoted batch back to the search pool
./target/release/import-accounts restore-search search-accounts.txt
```

`import-accounts import` (and the live admin upload form at `/admin/import`) accepts:

- `username:...:ct0:auth_token` or `username----...----auth_token----ct0` lines (40-hex `auth_token`, 160-hex `ct0`)
- `Cookie: auth_token=...; ct0=...` headers (or bare semicolon-separated cookie pairs)
- JSON objects or arrays (`{ "auth_token": "...", "ct0": "..." }`, `{ "cookies": ... }`, or browser cookie-editor JSON exports)
- Netscape HTTP cookie files

The running `teapot` process automatically reconciles newly imported accounts from PostgreSQL without needing a restart.

**Never commit** a real `sessions.jsonl`, `config/teapot.toml`, `proxies.txt`, `session-proxies.json`, or `donations.json`. Those paths are gitignored.

## Account Pools, Rate Limits, and Cache

- **Separate `search` and `view` pools**:
  - `view` accounts serve profiles, timelines, tweets, threads, lists, and communities. They are never used for search.
  - `search` accounts (`"pool": "search"` or imported into `search`) serve `SearchTimeline` only and are kept out of profile/view reads, except for self-service settings writes that turn off X's sensitive-media and search-safety filters.
  - Newly added search accounts are verified by a background audit before visitor traffic uses them, so you need at least one account in the `view` pool for profiles/tweets and at least one in the `search` pool if you want search to work.
- **`maxConcurrentReqs`** is **per account**, not global (default: `2`). An account already at its cap is skipped and another account is used immediately.
- **`clientBudget`** caps how fast one visitor IP can spend upstream X calls. Media and static assets are free.
- **No in-process HTML cache**: Pages are fetched fresh from X and cached downstream by your CDN (`s-maxage`), while identical concurrent upstream calls are coalesced in flight.
- **Search refusals, suspensions, and denylist**:
  - An empty search 404 briefly rests the account instead of permanently banning it; confirmed search refusals demote the account to the `view` pool, and a periodic background probe can reinstate demoted accounts if X allows them to search again.
  - Dead credentials are HTTP `401` or `403` with code `326`. Confirmed suspensions (`403` code `64`) leave search and retire if view reads also fail. Stale `ct0` tokens are refreshed from CSRF rejections or can be replaced from `/admin/stats`.
  - Queries rejected by X's Search Content Control denylist (`QueryDenylistedFailure`) are recorded in `search-denylist.json` (`TEAPOT_SEARCH_DENYLIST`) and automatically retried with `include:nativeretweets`.
- **Live GraphQL query IDs**: `teapot` fetches current GraphQL query IDs and feature flags from X's web client bundles and persists them to `graphql-ops.json` (`TEAPOT_GRAPHQL_OPS_FILE`).

## Proxies

### SOCKS5 exit pool

When `socksProxiesFile` is configured, API calls, script-chunk fetches, and media fetches (`/pic`, `/video`, `/gif`, round-robin across healthy exits) all leave through those exits so no upstream request uses the server's own IP. Without `socksProxiesFile`, media uses the optional HTTP `proxy` / `proxyAuth` setting (or direct).

```bash
cp proxies.example.txt proxies.txt
# one host:port:user:pass per line
```

```toml
# config/teapot.toml
socksProxiesFile   = "proxies.txt"
sessionProxiesFile = "session-proxies.json"
```

All lines in `proxies.txt` must share one hostname (different ports are the rotating exits). On startup `teapot` probes that host's A records (when there is more than one) and picks the lowest-ping IP. Each account stays on a stable port: existing pins in `sessionProxiesFile` (`{ "sessionId": port }`) are read if present (read-only), and all other or newly imported sessions deterministically map to a port from the sorted port list.

An exit that gets throttled at X's edge (HTML 429) or responds far slower than the rest of the fleet is parked temporarily with exponential backoff while traffic continues through the remaining healthy ports. Cloudflare IP blocks are written to `disabled-proxies.txt` (`TEAPOT_DISABLED_PROXIES_FILE`) and lapse automatically after 3 hours.

### Fallback proxies (direct mode)

When running directly without `socksProxiesFile`:

- `apiProxy` (with `proxyAuth`) is an HTTP proxy tried only after a direct request gets an HTML 429.
- `fallbackProxiesFile` accepts a list of `http://user:pass@host:port`, `socks5h://user:pass@host:port`, or `host:port:user:pass` (HTTP) exits sharing one gateway hostname. At startup `teapot` probes the gateway (requiring ≤20 ms RTT) and rotates through those exits on HTML 429s.

### Transport (`koon-core` & `scrape-socks`)

Outbound requests use `koon-core` with per-account browser profiles (Chrome, Edge, Firefox, Safari, Opera). At startup `teapot` handshakes with `x.com` to pick the newest version per engine that X accepts. API calls pre-seed `Alt-Svc` for `x.com` and `api.x.com` to negotiate **HTTP/3 (QUIC)** — including over SOCKS5 `UDP ASSOCIATE` — with automatic TCP/HTTP/2 fallback when UDP is unavailable on a proxy. Media fetches use HTTP/1.1.

The repo also includes `scrape-socks`, a local SOCKS5 forwarder supporting both TCP `CONNECT` and `UDP ASSOCIATE` (including chaining through Mullvad's in-tunnel SOCKS5 relay):

```bash
./target/release/scrape-socks /path/to/map-file
```

Map lines have the form `<port> <user> <password> <bind_ip> [via]`. It listens on `127.0.0.1` plus any addresses listed in `SCRAPE_SOCKS_LISTEN`.

## PostgreSQL

All tables are created automatically in the target database (default: `host=/var/run/postgresql user=teapot dbname=teapot`, overridden by `TEAPOT_LIMITS_DATABASE_URL`). There is no JSON fallback for accounts or rate limits — PostgreSQL must be reachable when `teapot` starts.

| Tables | Required? | Purpose |
| --- | --- | --- |
| `account_sessions`, `account_cookies`, `account_migrations`, `account_import_batches` | Yes | Account inventory, pool assignments, per-account cookie jars, migration markers, and import history |
| `session_limits` | Yes | One JSONB row per account for rate limits, refusals, suspensions, and filter flags (batched upserts) |
| `user_ids` | Best-effort | Lowercase screen name ↔ numeric id directory so profiles can request header and posts in parallel; loaded in the background into sharded maps (stays in-memory only if unavailable) |
| `stats_minute_buckets`, `stats_restrictions`, `stats_account_events`, `stats_search_account_minutes` | Best-effort | 30-day operational telemetry for `/admin/stats` (optional separate DB via `TEAPOT_STATS_DATABASE_URL`) |
| `pass_accounts`, `pass_orders`, `pass_sessions`, `pass_ips` | Best-effort | Optional paid search passes (`/search-pass`), whitelisted IPs, and the `admin` login used for `/admin/import` and `/admin/stats` |

## Running

```bash
# Development
cargo run --bin teapot

# Production
./target/release/teapot
```

The server listens on `http://0.0.0.0:8080` by default (`server.address` / `server.port` in `config/teapot.toml`).

### Environment variables

| Variable | Default | Purpose |
| --- | --- | --- |
| `TEAPOT_CONF_FILE` | `config/teapot.toml` | Path to configuration file |
| `TEAPOT_LIMITS_DATABASE_URL` | `host=/var/run/postgresql user=teapot dbname=teapot` | PostgreSQL connection string for accounts, limits, user IDs, and passes (and stats unless overridden) |
| `TEAPOT_STATS_DATABASE_URL` | *(falls back to `TEAPOT_LIMITS_DATABASE_URL`)* | Optional separate PostgreSQL connection string for telemetry |
| `TEAPOT_SESSIONS_FILE` | `sessions.jsonl` | Legacy sessions file read by `import-accounts migrate` |
| `TEAPOT_COOKIES_FILE` | `session-cookies.json` | Optional legacy cookies file read by `import-accounts migrate` |
| `TEAPOT_GRAPHQL_OPS_FILE` | `graphql-ops.json` | Persisted GraphQL query IDs and feature switches |
| `TEAPOT_SEARCH_DENYLIST` | `search-denylist.json` | Persisted denylisted search query rewrites |
| `TEAPOT_DISABLED_PROXIES_FILE` | `disabled-proxies.txt` | Temporarily disabled (Cloudflare-banned) SOCKS5 exit ports |
| `RUST_LOG` | `teapot=info,tower_http=info` | Tracing log filter |
| `SCRAPE_SOCKS_LISTEN` | `127.0.0.1` | Additional listen IPs for `scrape-socks` |

Donation providers can also be configured via environment variables instead of `config/teapot.toml`: `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`, `NOWPAYMENTS_API_KEY`, `NOWPAYMENTS_IPN_SECRET`, `LZT_ACCESS_TOKEN`, `LZT_MERCHANT_ID`, `LZT_SECRET_KEY`, and `LZT_CURRENCY`.

### Admin and operational routes

- `/import` — public pool totals and recent import batch counts (`/pool-counts` returns JSON)
- `/admin/import` — live account upload form (requires signing in via `/search-pass/login` as the `admin` account in `pass_accounts`)
- `/admin/stats` — operational dashboard, search account health, out/unverified account inspector, live probes, pool switching, and `ct0` replacement (same `admin` login)
- `/.health`, `/.sessions` — JSON debug endpoints when `config.enableDebug = true` and `config.debugToken` (≥32 chars) is sent as `Authorization: Bearer <token>`

## Getting Twitter Sessions

To use the Twitter/X API, you need session tokens from a logged-in account:

1. Log into Twitter/X in your browser
2. Open Developer Tools (F12) → Application (or Storage) → Cookies
3. Copy the values of `auth_token` and `ct0`
4. Add them to `sessions.jsonl` and run `./target/release/import-accounts migrate` (for initial setup), or import them later with `./target/release/import-accounts import <file> <search|view>` or `/admin/import`

## Project Structure

```
teapawt/
├── src/
│   ├── main.rs            # Server entry point and middleware stack
│   ├── config.rs          # Configuration loading and validation
│   ├── accounts.rs        # PostgreSQL account store, cookie jars, import parser
│   ├── account_repair.rs  # One-time startup state repairs
│   ├── stats.rs           # PostgreSQL operational telemetry store
│   ├── passes/            # Paid search pass store, checkout, IP whitelist
│   ├── donations.rs       # Stripe / NOWPayments / LZT donation ledger
│   ├── pressure.rs        # Pool-pressure detector for automatic Anti-DDoS gate
│   ├── transcode.rs       # Local/external GIF transcoding
│   ├── translation.rs     # Kagi Translate integration
│   ├── error.rs           # Error types and HTTP error pages
│   ├── types/             # Data structures (User, Tweet, Timeline, Session, Prefs)
│   ├── api/               # X API client, koon-core HTTP/3 client, SOCKS5 pool, parsers
│   ├── cache/             # Sharded Postgres-backed user-id store and GIF cache
│   ├── routes/            # HTTP route handlers (pages, search, RSS, embeds, admin)
│   ├── views/             # Maud HTML templates
│   ├── utils/             # Utilities (HMAC, host routing, formatters)
│   └── bin/
│       ├── import-accounts.rs  # Offline PostgreSQL account migration/import CLI
│       └── scrape-socks.rs     # Local SOCKS5 (TCP + UDP ASSOCIATE) forwarder
├── vendor/
│   └── xitter-txid/       # Patched transaction-id crate
├── public/                # Static assets (CSS, JS, fonts, icons, logo)
├── config/                # teapot.example.toml
├── proxies.example.txt    # Dummy SOCKS5 list
├── sessions.example.jsonl # Example session JSONL
└── Cargo.toml
```

## Embed Improvements

teapawt includes FxEmbed-style improvements for Discord embeds:

### Multiple Images

Discord can display all images from a tweet in a carousel. This works via an ActivityPub JSON endpoint at `/users/{username}/statuses/{id}` that returns media attachments.

### Video and GIF Playback

Videos can be played directly in Discord embeds using `twitter:player` meta tags:

- `twitter:player` - Embed URL
- `twitter:player:stream` - Direct MP4 URL
- `twitter:player:width/height` - Dimensions

Twitter MP4 "GIFs" can also be transcoded to real GIFs for inline Discord playback via `[gifTranscoding]` (local `ffmpeg` with disk cache, or an external transcoding domain).

### Image Dimensions

All images include `og:image:width` and `og:image:height` meta tags for proper layout.

## Donate

Running the public instance costs money for servers, bandwidth and the
accounts and exits that keep it working. If nitter.cf is useful to you,
donations help keep it online. Nothing is required.

| Coin | Address |
| ---- | ------- |
| BTC  | `bc1qtm4rep7kyp2p28hlkfktpnd0j7uew47vnt2mev` |
| LTC  | `ltc1q9xlaysse0mpfwf025esd69kt9zu3md22gh23du` |
| ETH  | `0x6abA18D203520c8F90ca63eE7426a7bC345D98e6` |
| SOL  | `E5ekoiauCR8yiNp451dmuPzhUxo84P5x3m9WJBK5kxZe` |
| XRP  | `r3jYXNdMk2Vz5hXhKCKE9re8uz93JQFG1r` |
| DOGE | `DSBq5jRWkiwmEh1G7zBwmQDgRkvRr61fmb` |
| XMR  | `85CTwq5wGjWZw5GgzgwZPrHyZxpLXMK5oZSSSk5j4x9w1bpqzisaWAUZYaAPfh6rP2MZJcaFqF1XmEzkw6YCqJSS2Txu6Z8` |

## License

[GNU Affero General Public License v3.0](LICENSE)
