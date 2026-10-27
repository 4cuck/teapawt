# teapawt

A privacy-focused Twitter/X frontend written in Rust.

Public instances: [nitter.cf](https://nitter.cf), [nitter.gq](https://nitter.gq), [xitter.cf](https://xitter.cf), [xitter.gq](https://xitter.gq), [teapaw.org](https://teapaw.org), [teapawt.net](https://teapawt.net), and [teapawt.org](https://teapawt.org).

Live status for those instances: [status.d420.net](https://status.d420.net). Scraping is allowed on them. Ask the operator before scraping any other Nitter instance.

## Features

- **Privacy-focused**: No third-party JavaScript, tracking, ads, or remote fonts
- **RSS feeds**: Subscribe to any user's tweets
- **SOCKS5 exits for GraphQL**: Pin each cookie session to one proxy port so
  its apparent IP stays stable
- **PostgreSQL**: one row per account for rate limits, and a screen name to
  numeric id so the next profile can request the header and the posts together.
  Pages themselves are not stored in the process
- **FxEmbed-style Discord embeds**:
  - Multiple images displayed in carousel on mobile
  - Videos play directly in the embed
  - Proper image dimensions for better layout
  - ActivityPub JSON endpoint for rich embeds

## Requirements

- Rust 1.96.1+
- Twitter/X session tokens for API access
- PostgreSQL, for account rate limits and user ids. Startup continues if it
  cannot be reached
- Optional: SOCKS5 endpoints if X blocks the server's own IP

## Building

```bash
cargo build --release
```

The crate and binary stay named `teapot` so existing configs and process
managers keep working.

## Configuration

1. Copy the example config:

```bash
cp config/teapot.example.toml config/teapot.toml
```

2. Edit `config/teapot.toml`. Set `title = "teapawt"` (or whatever you want
   shown in the navbar).

3. Behind nginx or another TLS terminator, set `https = true` and
   `publicPort = 443` so prefs bookmarks, RSS, and `og:image` links omit the
   listen port (`8471` or whatever `port` is).

4. Create a sessions file with your Twitter/X credentials:

```bash
cp sessions.example.jsonl sessions.jsonl
# Edit sessions.jsonl with your auth_token and ct0 from browser cookies
```

Cookie sessions use `auth_token` and `ct0`. OAuth sessions use
`oauth_token` and `oauth_secret`. See `sessions.example.jsonl` for the JSONL
shape.

**Never commit** a real `sessions.jsonl`, `config/teapot.toml`, `proxies.txt`,
or `session-proxies.json`. Those paths are gitignored.

### Proxies

X GraphQL (search, profiles, tweets) can go out through SOCKS5. Media (`/pic`,
`/video`, `/gif`) stays on the optional HTTP `proxy` and does not use the
SOCKS list.

```bash
cp proxies.example.txt proxies.txt
# one host:port:user:pass per line
```

```toml
# config/teapot.toml
socksProxiesFile   = "proxies.txt"
sessionProxiesFile = "session-proxies.json"
```

All lines must share one hostname (different ports are the rotating exits).
On startup teapawt DNS-probes that host, keeps the lowest-ping A record, and
pins each session id to one port. Pins are rewritten to `sessionProxiesFile`
as `{ "sessionId": port }` only — credentials stay in `proxies.txt`.

`apiProxy` is an HTTP/HTTPS proxy for GraphQL when you are not using SOCKS.
It is ignored when `socksProxiesFile` is set.

Tunnels to `x.com` are HTTP/1.1 over TLS with keep-alive, `TCP_NODELAY`, and
rustls session resumption. Idle tunnels are reused for a short window so a
second search does not pay a new SOCKS + TLS handshake.

### Rate limits and cache

- `maxConcurrentReqs` is **per cookie session**, not global. At `2`, an
  account that already has two calls in flight is skipped and the next
  account is used immediately. A crawl stays at two in flight per account
  across the whole pool. Raising it into the millions lets one burst pile
  onto the few accounts already known to work, which is what produces a
  cliff of 429s.
- `clientBudget` caps how fast one visitor IP can spend X calls. Media and
  static files are free. SearchTimeline costs 2 tokens.
- HTML is not kept in the process. A profile is fetched from X, and a repeat
  view is Cloudflare's `s-maxage` on the response.

Some otherwise valid cookie accounts are not allowed to search: X answers
their `SearchTimeline` calls with an empty HTTP 404. teapawt retries the same
search on another session. When that session returns results, the first
account is recorded and permanently left out of search rotation; it can still
serve other operations. If both sessions return nothing, teapawt treats it as
an upstream/query-id problem instead of blaming either account. Dead cookies
are 401, or 403 with code 326 — not those empty 404s.

### PostgreSQL

Two tables in a database named `teapot`, owned by a login role named `teapot`.
The process creates the tables. Nothing here is a copy of a page.

| Table | What it holds |
| --- | --- |
| `session_limits` | One jsonb row per account: remaining calls, refusals, and the filter flags. A change updates that row. |
| `user_ids` | Lowercase screen name to the numeric id `UserTweets` requires. A known name requests the header and the posts in the same wave. |

The default connection is peer auth on the local socket, with no password in
the repo:

```
host=/var/run/postgresql user=teapot dbname=teapot
```

`TEAPOT_LIMITS_DATABASE_URL` overrides that string. The OS user that runs
teapawt has to be allowed to log in as `teapot` (a `pg_ident` map when that
user is not `teapot`).

```sql
CREATE ROLE teapot LOGIN;
CREATE DATABASE teapot OWNER teapot;
```

Rate-limit state still has a JSON fallback, `session-limits.json`, or
whatever `TEAPOT_SESSION_STATE_FILE` points at. An empty value turns that
store off. The first time Postgres is reachable and `session_limits` is
empty, that file is imported and later updates go to the one row. If Postgres
is down at startup, the file is used until it is back. User ids learned while
it is down stay in memory for that process.

Some queries fail with X error 214 (`Query is denylisted in Search Content
Control tool`). That is X's platform denylist, not a teapawt setting.

## Running

```bash
# Development
cargo run

# Production
./target/release/teapot
```

The server listens on `http://localhost:8080` by default (`address` / `port`
in the config).

## Getting Twitter Sessions

To use the Twitter API, you need session tokens from a logged-in Twitter account:

1. Log into Twitter/X in your browser
2. Open Developer Tools (F12) → Application → Cookies
3. Copy the values of `auth_token` and `ct0`
4. Add them to `sessions.jsonl`

## Project Structure

```
teapawt/
├── src/
│   ├── main.rs          # Entry point
│   ├── config.rs        # Configuration
│   ├── error.rs         # Error types
│   ├── types/           # Data structures (User, Tweet, Timeline, etc.)
│   ├── api/             # Twitter API client, SOCKS5 pool, OAuth, parsing
│   ├── cache/           # Page cache (off in the server) and the user-id store
│   ├── routes/          # HTTP route handlers
│   ├── views/           # Maud HTML templates
│   └── utils/           # Utilities (HMAC, formatters)
├── public/              # Static assets (CSS, JS, fonts, logo)
├── config/              # teapot.example.toml only
├── proxies.example.txt  # Dummy SOCKS5 list
├── sessions.example.jsonl
└── Cargo.toml
```

## Embed Improvements

This rewrite includes FxEmbed-style improvements for Discord embeds:

### Multiple Images

Discord can display all images from a tweet in a carousel. This works via an ActivityPub JSON endpoint at `/users/{username}/statuses/{id}` that returns media attachments.

### Video Playback

Videos can be played directly in Discord embeds using `twitter:player` meta tags:

- `twitter:player` - Embed URL
- `twitter:player:stream` - Direct MP4 URL
- `twitter:player:width/height` - Dimensions

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
