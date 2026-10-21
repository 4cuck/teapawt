//! Discovery documents and read tools for AI agents.
//!
//! The public site has no accounts, so this does not pretend to be an OAuth
//! server. Agents read pages as markdown, or call the read-only MCP endpoint.

use axum::{
   Json, Router,
   body::Body,
   extract::{Request, State},
   http::{HeaderMap, HeaderValue, StatusCode, header, uri::PathAndQuery},
   middleware::Next,
   response::{IntoResponse, Response},
   routing::{get, post},
};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};

use crate::{AppState, error::Error, types::Tweet};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const DISCOVERY_LINKS: &str = "\
</.well-known/api-catalog>; rel=\"api-catalog\", \
</openapi.json>; rel=\"service-desc\", \
</llms.txt>; rel=\"service-doc\", \
</auth.md>; rel=\"describedby\", \
</.well-known/mcp/server-card.json>; rel=\"describedby\", \
</.well-known/agent-card.json>; rel=\"describedby\", \
</.well-known/agent-skills/index.json>; rel=\"describedby\", \
</.well-known/ai-catalog.json>; rel=\"describedby\", \
</.well-known/poppy.json>; rel=\"describedby\"";

const SKILL_MD: &str = r#"# Read public posts on teapawt

teapawt (xitter.cf, also nitter.cf) is a public read-only frontend for X.
No account or API key is required.

## Pages

Request any public page with `Accept: text/markdown`. The same page is also
available at its path plus `/index.md`.

- Search posts: `GET /search?f=tweets&q={query}`
- Search profiles: `GET /search?f=users&q={query}`
- Profile: `GET /{username}`
- Post and replies: `GET /{username}/status/{id}`
- Home, about, donate: `GET /`, `GET /about`, `GET /donate`

## Tools

`POST /mcp` speaks Model Context Protocol over Streamable HTTP.
Tools: `search_posts`, `get_profile`, `get_post`.
The server card is `/.well-known/mcp/server-card.json`.

`POST /a2a` accepts an A2A `message/send` whose text is a search, an
`@username`, or a post URL.
"#;

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/llms.txt", get(llms_txt))
      .route("/auth.md", get(auth_md))
      .route("/openapi.json", get(openapi))
      .route("/.well-known/api-catalog", get(api_catalog))
      .route("/.well-known/mcp/server-card.json", get(mcp_card))
      .route("/.well-known/agent-card.json", get(agent_card))
      .route("/.well-known/agent-skills/index.json", get(skills_index))
      .route("/.well-known/agent-skills/read-teapawt/SKILL.md", get(skill_md))
      .route("/.well-known/ai-catalog.json", get(ai_catalog))
      .route("/.well-known/poppy.json", get(poppy))
      .route("/.well-known/oauth-protected-resource", get(protected_resource))
      .route("/.well-known/oauth-authorization-server", get(authorization_server))
      .route("/.well-known/openid-configuration", get(authorization_server))
      .route("/.well-known/jwks.json", get(jwks))
      .route("/oauth/authorize", get(oauth_public))
      .route("/oauth/token", get(oauth_public).post(oauth_public))
      .route("/mcp", post(mcp).options(preflight))
      .route("/a2a", post(a2a).options(preflight))
}

/// Serve markdown when the client asks for it, and advertise discovery documents.
pub async fn negotiate(mut request: Request, next: Next) -> Response {
   let markdown = wants_markdown(request.headers()) || rewrite_index_md(&mut request);
   let mut response = next.run(request).await;
   response.headers_mut().insert(
      header::LINK,
      HeaderValue::from_static(DISCOVERY_LINKS),
   );
   // Caches must not reuse an HTML page for a markdown request.
   if is_html(response.headers()) {
      append_vary(response.headers_mut());
   }
   if !markdown || !is_html(response.headers()) {
      return response;
   }

   let (mut parts, body) = response.into_parts();
   let bytes = match body.collect().await {
      Ok(collected) => collected.to_bytes(),
      Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
   };
   let markdown = html_to_markdown(&String::from_utf8_lossy(&bytes));
   let tokens = markdown.chars().count().div_ceil(4).max(1).to_string();
   parts.headers.insert(
      header::CONTENT_TYPE,
      HeaderValue::from_static("text/markdown; charset=utf-8"),
   );
   if let Ok(value) = HeaderValue::from_str(&tokens) {
      parts.headers.insert(
         axum::http::HeaderName::from_static("x-markdown-tokens"),
         value,
      );
   }
   parts.headers.remove(header::CONTENT_LENGTH);
   append_vary(&mut parts.headers);
   Response::from_parts(parts, Body::from(markdown))
}

fn rewrite_index_md(request: &mut Request) -> bool {
   let Some(path) = strip_index_md(request.uri().path()) else {
      return false;
   };
   let query = request
      .uri()
      .query()
      .map(|query| format!("?{query}"))
      .unwrap_or_default();
   let Ok(path_and_query) = format!("{path}{query}").parse::<PathAndQuery>() else {
      return false;
   };
   let mut parts = request.uri().clone().into_parts();
   parts.path_and_query = Some(path_and_query);
   let Ok(uri) = axum::http::Uri::from_parts(parts) else {
      return false;
   };
   *request.uri_mut() = uri;
   true
}

fn strip_index_md(path: &str) -> Option<String> {
   if path == "/index.md" {
      return Some("/".to_owned());
   }
   path.strip_suffix("/index.md")
      .filter(|rest| !rest.is_empty())
      .map(str::to_owned)
}

fn is_html(headers: &HeaderMap) -> bool {
   headers
      .get(header::CONTENT_TYPE)
      .and_then(|value| value.to_str().ok())
      .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"))
}

fn append_vary(headers: &mut HeaderMap) {
   let Some(existing) = headers.get(header::VARY).and_then(|value| value.to_str().ok()) else {
      headers.insert(header::VARY, HeaderValue::from_static("Accept"));
      return;
   };
   if existing
      .split(',')
      .any(|part| part.trim().eq_ignore_ascii_case("accept"))
   {
      return;
   }
   if let Ok(value) = HeaderValue::from_str(&format!("{existing}, Accept")) {
      headers.insert(header::VARY, value);
   }
}

/// `text/markdown` wins when it is present and at least as preferred as HTML.
fn wants_markdown(headers: &HeaderMap) -> bool {
   let Some(accept) = headers.get(header::ACCEPT).and_then(|value| value.to_str().ok()) else {
      return false;
   };
   let mut markdown = None;
   let mut html = None;
   for part in accept.split(',') {
      let mut pieces = part.split(';');
      let mime = pieces.next().unwrap_or("").trim().to_ascii_lowercase();
      let mut quality = 1.0_f32;
      for piece in pieces {
         if let Some(value) = piece.trim().strip_prefix("q=")
            && let Ok(parsed) = value.parse::<f32>()
         {
            quality = parsed;
         }
      }
      match mime.as_str() {
         "text/markdown" => markdown = Some(quality),
         "text/html" | "*/*" => html = Some(html.unwrap_or(quality).max(quality)),
         _ => {}
      }
   }
   match (markdown, html) {
      (Some(markdown), Some(html)) => markdown > 0.0 && markdown >= html,
      (Some(markdown), None) => markdown > 0.0,
      _ => false,
   }
}

fn html_to_markdown(html: &str) -> String {
   let bytes = html.as_bytes();
   let mut out = String::new();
   let mut index = 0;
   while index < bytes.len() {
      if bytes[index] == b'<' {
         if bytes[index..].starts_with(b"<!--") {
            index = skip_until(bytes, index + 4, b"-->");
            continue;
         }
         let start = index + 1;
         let end = bytes[start..]
            .iter()
            .position(|byte| *byte == b'>')
            .map_or(bytes.len(), |offset| start + offset);
         let raw = &bytes[start..end.min(bytes.len())];
         let tag = tag_name(raw);
         let closing = raw.first() == Some(&b'/');
         index = (end + 1).min(bytes.len());
         if matches!(tag.as_str(), "script" | "style" | "svg" | "noscript") && !closing {
            let closer = format!("</{tag}");
            index = skip_until_ignore_case(bytes, index, closer.as_bytes());
         }
         match tag.as_str() {
            "h1" if !closing => out.push_str("\n\n# "),
            "h2" if !closing => out.push_str("\n\n## "),
            "h3" if !closing => out.push_str("\n\n### "),
            "h4" | "h5" | "h6" if !closing => out.push_str("\n\n#### "),
            "p" | "div" | "section" | "article" | "li" | "tr" | "br" | "h1" | "h2" | "h3"
            | "h4" | "h5" | "h6" | "blockquote"
               if closing || tag == "br" =>
            {
               out.push_str("\n\n");
            },
            "li" if !closing => out.push_str("\n- "),
            "a" if !closing => {
               if let Some(href) = attr(raw, "href") {
                  let text = read_element_text(bytes, &mut index, "a");
                  if !text.is_empty() {
                     out.push('[');
                     out.push_str(&text);
                     out.push_str("](");
                     out.push_str(&href);
                     out.push(')');
                  }
               }
            },
            _ => {}
         }
         continue;
      }
      let start = index;
      while index < bytes.len() && bytes[index] != b'<' {
         index += 1;
      }
      let text = decode_entities(&String::from_utf8_lossy(&bytes[start..index]));
      let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
      if !collapsed.is_empty() {
         if out.ends_with(|ch: char| !ch.is_whitespace()) {
            out.push(' ');
         }
         out.push_str(&collapsed);
      }
   }
   let mut compact = String::new();
   let mut blank = 0;
   for line in out.lines() {
      let line = line.trim();
      if line.is_empty() {
         blank += 1;
         if blank <= 1 {
            compact.push('\n');
         }
      } else {
         blank = 0;
         compact.push_str(line);
         compact.push('\n');
      }
   }
   compact.trim().to_owned()
}

fn tag_name(raw: &[u8]) -> String {
   let raw = raw.strip_prefix(b"/").unwrap_or(raw);
   let end = raw
      .iter()
      .position(|byte| byte.is_ascii_whitespace() || *byte == b'/' || *byte == b'>')
      .unwrap_or(raw.len());
   String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase()
}

fn attr(raw: &[u8], name: &str) -> Option<String> {
   let raw = String::from_utf8_lossy(raw);
   let lower = raw.to_ascii_lowercase();
   let needle = format!("{name}=");
   let start = lower.find(&needle)? + needle.len();
   let rest = raw[start..].trim_start();
   let quote = rest.chars().next()?;
   if quote == '"' || quote == '\'' {
      let body = rest[1..].split(quote).next().unwrap_or("");
      Some(decode_entities(body))
   } else {
      Some(decode_entities(
         rest.split_whitespace().next().unwrap_or(""),
      ))
   }
}

fn read_element_text(bytes: &[u8], index: &mut usize, name: &str) -> String {
   let start = *index;
   let closer = format!("</{name}");
   *index = skip_until_ignore_case(bytes, *index, closer.as_bytes());
   let inner = &bytes[start..(*index).min(bytes.len())];
   let mut text = String::new();
   let mut cursor = 0;
   while cursor < inner.len() {
      if inner[cursor] == b'<' {
         cursor = inner[cursor..]
            .iter()
            .position(|byte| *byte == b'>')
            .map_or(inner.len(), |offset| cursor + offset + 1);
         continue;
      }
      let from = cursor;
      while cursor < inner.len() && inner[cursor] != b'<' {
         cursor += 1;
      }
      text.push_str(&String::from_utf8_lossy(&inner[from..cursor]));
   }
   decode_entities(&text)
      .split_whitespace()
      .collect::<Vec<_>>()
      .join(" ")
}

fn skip_until(bytes: &[u8], from: usize, marker: &[u8]) -> usize {
   bytes[from..]
      .windows(marker.len())
      .position(|window| window == marker)
      .map_or(bytes.len(), |offset| from + offset + marker.len())
}

fn skip_until_ignore_case(bytes: &[u8], from: usize, marker: &[u8]) -> usize {
   if marker.is_empty() {
      return from;
   }
   let mut index = from;
   while index + marker.len() <= bytes.len() {
      if bytes[index..index + marker.len()]
         .iter()
         .zip(marker)
         .all(|(byte, expected)| byte.to_ascii_lowercase() == expected.to_ascii_lowercase())
      {
         let after = index + marker.len();
         return bytes[after..]
            .iter()
            .position(|byte| *byte == b'>')
            .map_or(bytes.len(), |offset| after + offset + 1);
      }
      index += 1;
   }
   bytes.len()
}

fn decode_entities(text: &str) -> String {
   let mut out = String::with_capacity(text.len());
   let mut rest = text;
   while let Some(start) = rest.find('&') {
      out.push_str(&rest[..start]);
      rest = &rest[start..];
      let Some(end) = rest.find(';') else {
         out.push_str(rest);
         return out;
      };
      let entity = &rest[1..end];
      let ch = match entity {
         "amp" => "&",
         "lt" => "<",
         "gt" => ">",
         "quot" => "\"",
         "apos" | "#39" => "'",
         "nbsp" => " ",
         _ => "",
      };
      if ch.is_empty() {
         out.push('&');
         rest = &rest[1..];
      } else {
         out.push_str(ch);
         rest = &rest[end + 1..];
      }
   }
   out.push_str(rest);
   out
}

fn origin(state: &AppState) -> String {
   state.config.url_prefix().trim_end_matches('/').to_owned()
}

fn json_doc(body: Value, content_type: &'static str) -> Response {
   (
      [
         (header::CONTENT_TYPE, content_type),
         (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
         (header::CACHE_CONTROL, "public, max-age=300"),
      ],
      body.to_string(),
   )
      .into_response()
}

fn markdown_doc(body: &'static str) -> Response {
   (
      [
         (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
         (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
         (header::CACHE_CONTROL, "public, max-age=300"),
      ],
      body,
   )
      .into_response()
}

async fn llms_txt(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   let body = format!(
      "\
# teapawt

> Public read-only frontend for X, served at {origin}. No account is required.
> The same site also answers on nitter.cf.

Send `Accept: text/markdown` on any page, or append `/index.md` to the path.

## Read

- [Home]({origin}/): where to start
- [About]({origin}/about): what this instance is
- [Search posts]({origin}/search?f=tweets&q=): set `q`
- [Profile]({origin}/github): `GET /{{username}}`
- [Post]({origin}/github/status/2107896916595884177): `GET /{{username}}/status/{{id}}`

## Agents

- [How to read]({origin}/llms.txt)
- [auth.md]({origin}/auth.md): no credentials required
- [OpenAPI]({origin}/openapi.json)
- [API catalog]({origin}/.well-known/api-catalog)
- [MCP server card]({origin}/.well-known/mcp/server-card.json)
- [Agent card]({origin}/.well-known/agent-card.json)
- [Skills]({origin}/.well-known/agent-skills/index.json)
"
   );
   (
      [
         (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
         (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
         (header::CACHE_CONTROL, "public, max-age=300"),
      ],
      body,
   )
      .into_response()
}

async fn auth_md() -> Response {
   markdown_doc(
      "\
# auth.md

teapawt is a public read-only frontend for X. Agents do not register and do not send credentials.

## Audience

Any agent may read public pages and call the read tools. The identity type is anonymous.

## Registration

There is no account to create. `/.well-known/oauth-authorization-server` advertises `agent_auth` with `identity_types_supported: [\"anonymous\"]`. The register URI is this document. Do not send a bearer token. `/oauth/token` does not issue one.

## Requests

Ask for markdown with `Accept: text/markdown`, or append `/index.md` to a path.

- `GET /search?f=tweets&q={query}`
- `GET /{username}`
- `GET /{username}/status/{id}`
- `POST /mcp` with tools `search_posts`, `get_profile`, and `get_post`

The server card is at `/.well-known/mcp/server-card.json`.
",
   )
}

async fn protected_resource(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "resource": format!("{origin}/"),
         "authorization_servers": [origin],
         "scopes_supported": [],
         "bearer_methods_supported": ["header"]
      }),
      "application/json",
   )
}

async fn authorization_server(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "issuer": origin,
         "authorization_endpoint": format!("{origin}/oauth/authorize"),
         "token_endpoint": format!("{origin}/oauth/token"),
         "jwks_uri": format!("{origin}/.well-known/jwks.json"),
         "grant_types_supported": ["authorization_code"],
         "response_types_supported": ["code"],
         "code_challenge_methods_supported": ["S256"],
         "agent_auth": {
            "skill": format!("{origin}/auth.md"),
            "register_uri": format!("{origin}/auth.md"),
            "identity_types_supported": ["anonymous"],
            "anonymous": {
               "credential_types_supported": ["none"],
               "claim_uri": format!("{origin}/auth.md")
            }
         }
      }),
      "application/json",
   )
}

async fn jwks() -> Response {
   json_doc(json!({"keys": []}), "application/json")
}

async fn oauth_public() -> Response {
   json_doc(
      json!({
         "error": "public_resource",
         "error_description": "xitter.cf does not issue access tokens. Read any public page with Accept: text/markdown."
      }),
      "application/json",
   )
}

async fn openapi(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "openapi": "3.1.0",
         "info": {
            "title": "teapawt",
            "version": VERSION,
            "description": "Public read-only X frontend. Send Accept: text/markdown for a markdown page."
         },
         "servers": [{ "url": origin }],
         "paths": {
            "/search": {
               "get": {
                  "operationId": "search",
                  "parameters": [
                     {"name": "q", "in": "query", "required": true, "schema": {"type": "string"}},
                     {"name": "f", "in": "query", "schema": {"type": "string", "enum": ["tweets", "users", "media"]}}
                  ],
                  "responses": {"200": {"description": "Search results"}}
               }
            },
            "/{username}": {
               "get": {
                  "operationId": "profile",
                  "parameters": [{"name": "username", "in": "path", "required": true, "schema": {"type": "string"}}],
                  "responses": {"200": {"description": "Profile and recent posts"}}
               }
            },
            "/{username}/status/{id}": {
               "get": {
                  "operationId": "post",
                  "parameters": [
                     {"name": "username", "in": "path", "required": true, "schema": {"type": "string"}},
                     {"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}
                  ],
                  "responses": {"200": {"description": "Post and replies"}}
               }
            },
            "/mcp": {
               "post": {
                  "operationId": "mcp",
                  "description": "Model Context Protocol Streamable HTTP.",
                  "responses": {"200": {"description": "JSON-RPC result"}}
               }
            }
         }
      }),
      "application/json",
   )
}

async fn api_catalog(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "linkset": [{
            "anchor": origin,
            "service-desc": [{
               "href": format!("{origin}/openapi.json"),
               "type": "application/vnd.oai.openapi+json"
            }],
            "service-doc": [{
               "href": format!("{origin}/llms.txt"),
               "type": "text/plain"
            }],
            "status": [{ "href": format!("{origin}/") }]
         }]
      }),
      "application/linkset+json",
   )
}

fn tool_specs() -> Value {
   json!([
      {
         "name": "search_posts",
         "title": "Search posts",
         "description": "Search public posts. Query can include words, #hashtags, or from:username.",
         "inputSchema": {
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
         }
      },
      {
         "name": "get_profile",
         "title": "Read a profile",
         "description": "Read a public profile and its recent posts.",
         "inputSchema": {
            "type": "object",
            "properties": {"username": {"type": "string"}},
            "required": ["username"]
         }
      },
      {
         "name": "get_post",
         "title": "Read a post",
         "description": "Read one public post by its numeric id.",
         "inputSchema": {
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"]
         }
      }
   ])
}

async fn mcp_card(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "version": "1.0",
         "protocolVersion": "2025-06-18",
         "serverInfo": {
            "name": "teapawt",
            "title": "teapawt",
            "version": VERSION
         },
         "description": "Read public X posts, profiles, and search results. No account is required.",
         "transport": {
            "type": "streamable-http",
            "endpoint": format!("{origin}/mcp")
         },
         "authentication": {"required": false},
         "capabilities": {"tools": {}},
         "tools": tool_specs()
      }),
      "application/json",
   )
}

async fn agent_card(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "name": "teapawt",
         "description": "Reads public X posts and profiles through xitter.cf. It cannot post, like, or sign in.",
         "version": VERSION,
         "url": format!("{origin}/a2a"),
         "supportedInterfaces": [{
            "url": format!("{origin}/a2a"),
            "protocol": "JSONRPC",
            "protocolBinding": "JSONRPC",
            "protocolVersion": "1.0"
         }],
         "capabilities": {"streaming": false},
         "defaultInputModes": ["text/plain"],
         "defaultOutputModes": ["text/plain", "text/markdown"],
         "skills": [{
            "id": "read-posts",
            "name": "Read public posts",
            "description": "Search posts, open a profile, or read a post. Send the query, @username, or post URL as text."
         }]
      }),
      "application/json",
   )
}

async fn skills_index(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "$schema": "https://schemas.agentskills.io/discovery/0.2.0/schema.json",
         "skills": [{
            "name": "read-teapawt",
            "type": "skill-md",
            "description": "Read public X posts, profiles, and search results through teapawt without an account.",
            "url": format!("{origin}/.well-known/agent-skills/read-teapawt/SKILL.md"),
            "digest": skill_digest()
         }]
      }),
      "application/json",
   )
}

async fn skill_md() -> Response {
   markdown_doc(SKILL_MD)
}

async fn poppy(State(state): State<AppState>, headers: HeaderMap) -> Response {
   let domain = requested_domain(&headers, &state);
   json_doc(
      json!({
         "protocol_version": "0.1",
         "organization": {
            "name": "teapawt",
            "domain": domain
         },
         "web": {}
      }),
      "application/json",
   )
}

fn requested_domain(headers: &HeaderMap, state: &AppState) -> String {
   let forwarded = headers
      .get("x-forwarded-host")
      .or_else(|| headers.get(header::HOST))
      .and_then(|value| value.to_str().ok())
      .unwrap_or("")
      .split(',')
      .next()
      .unwrap_or("")
      .trim()
      .split(':')
      .next()
      .unwrap_or("");
   let host = forwarded.strip_prefix("www.").unwrap_or(forwarded);
   if matches!(host, "xitter.cf" | "nitter.cf") {
      host.to_owned()
   } else {
      state
         .config
         .server
         .hostname
         .trim_start_matches("www.")
         .to_owned()
   }
}

async fn ai_catalog(State(state): State<AppState>) -> Response {
   let origin = origin(&state);
   json_doc(
      json!({
         "specVersion": "1.0",
         "host": {
            "displayName": "teapawt",
            "identifier": "did:web:xitter.cf"
         },
         "entries": [
            {
               "identifier": "urn:air:xitter.cf:server:read",
               "displayName": "teapawt reader",
               "type": "application/mcp-server-card+json",
               "url": format!("{origin}/.well-known/mcp/server-card.json"),
               "representativeQueries": [
                  "search recent public posts about rust",
                  "read the github profile",
                  "open post 2107896916595884177"
               ]
            },
            {
               "identifier": "urn:air:xitter.cf:skill:read-teapawt",
               "displayName": "Read public posts",
               "type": "text/markdown",
               "url": format!("{origin}/.well-known/agent-skills/read-teapawt/SKILL.md"),
               "representativeQueries": [
                  "how do I read a public post without signing in",
                  "what accept header returns markdown"
               ]
            }
         ]
      }),
      "application/json",
   )
}

async fn preflight() -> Response {
   (
      [
         (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
         (header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS"),
         (
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "content-type, accept, mcp-protocol-version",
         ),
         (header::ACCESS_CONTROL_MAX_AGE, "86400"),
      ],
      StatusCode::NO_CONTENT,
   )
      .into_response()
}

async fn mcp(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
   rpc(&state, body, true).await
}

async fn a2a(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
   rpc(&state, body, false).await
}

async fn rpc(state: &AppState, body: Value, mcp: bool) -> Response {
   if body.get("id").is_none_or(Value::is_null) {
      return StatusCode::ACCEPTED.into_response();
   }
   let id = body.get("id").cloned().unwrap_or(Value::Null);
   let method = body.get("method").and_then(Value::as_str).unwrap_or("");
   let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
   let result = if mcp {
      match method {
         "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "teapawt", "version": VERSION}
         })),
         "tools/list" => Ok(json!({"tools": tool_specs()})),
         "tools/call" => mcp_call(state, &params).await,
         "ping" => Ok(json!({})),
         _ => Err(format!("unknown method {method}")),
      }
   } else if method == "message/send" {
      a2a_message(state, &params).await.map(|text| {
         json!({
            "id": format!("teapawt-{}", id),
            "status": {"state": "completed"},
            "artifacts": [{"parts": [{"kind": "text", "text": text}]}]
         })
      })
   } else {
      Err(format!("unknown method {method}"))
   };
   let payload = match result {
      Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
      Err(message) => json!({
         "jsonrpc": "2.0",
         "id": id,
         "error": {"code": -32601, "message": message}
      }),
   };
   (
      [
         (header::CONTENT_TYPE, "application/json"),
         (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
      ],
      payload.to_string(),
   )
      .into_response()
}

async fn mcp_call(state: &AppState, params: &Value) -> Result<Value, String> {
   let name = params.get("name").and_then(Value::as_str).unwrap_or("");
   let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
   let text = match name {
      "search_posts" => {
         let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
         if query.is_empty() || query.len() > 400 {
            return Err("query is required".into());
         }
         search_text(state, query).await
      },
      "get_profile" => {
         let username = clean_username(args.get("username").and_then(Value::as_str).unwrap_or(""));
         if username.is_empty() {
            return Err("username is required".into());
         }
         profile_text(state, &username).await
      },
      "get_post" => {
         let id = args.get("id").and_then(Value::as_str).unwrap_or("").trim();
         if id.is_empty() || id.len() > 19 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("id must be the numeric post id".into());
         }
         post_text(state, id).await
      },
      _ => return Err(format!("unknown tool {name}")),
   };
   match text {
      Ok(text) => Ok(json!({"content": [{"type": "text", "text": text}], "isError": false})),
      Err(err) => Ok(json!({"content": [{"type": "text", "text": err}], "isError": true})),
   }
}

async fn a2a_message(state: &AppState, params: &Value) -> Result<String, String> {
   let text = message_text(params);
   if text.is_empty() {
      return Err("message text is required".into());
   }
   if let Some(id) = text
      .split("/status/")
      .nth(1)
      .map(|rest| rest.split(|ch: char| !ch.is_ascii_digit()).next().unwrap_or(""))
      && !id.is_empty()
   {
      return post_text(state, id).await;
   }
   if let Some(username) = text.trim().strip_prefix('@')
      && !username.contains(char::is_whitespace)
   {
      return profile_text(state, &clean_username(username)).await;
   }
   search_text(state, text.trim()).await
}

fn message_text(params: &Value) -> String {
   params
      .pointer("/message/parts")
      .and_then(Value::as_array)
      .map(|parts| {
         parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ")
      })
      .unwrap_or_default()
}

fn clean_username(username: &str) -> String {
   username
      .trim()
      .trim_start_matches('@')
      .chars()
      .take(15)
      .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
      .collect()
}

async fn search_text(state: &AppState, query: &str) -> Result<String, String> {
   let timeline = state
      .api
      .search(query, None, "Latest")
      .await
      .map_err(brief)?;
   let origin = origin(state);
   let mut out = format!("# Search: {query}\n\n");
   let mut count = 0;
   for group in timeline.content {
      for tweet in group {
         if count == 12 {
            break;
         }
         out.push_str(&format_tweet(&origin, &tweet));
         out.push_str("\n\n");
         count += 1;
      }
   }
   if count == 0 {
      out.push_str("No posts matched.\n");
   }
   Ok(out)
}

async fn profile_text(state: &AppState, username: &str) -> Result<String, String> {
   let profile = state.api.get_profile(username, None).await.map_err(brief)?;
   let origin = origin(state);
   let user = &profile.user;
   let mut out = format!(
      "# @{}\n{}\n\n{}\n\n{} followers, {} following, {} posts\n{}\n\n## Recent posts\n\n",
      user.username,
      user.fullname,
      user.bio.trim(),
      user.followers,
      user.following,
      user.tweets,
      format!("{origin}/{}", user.username)
   );
   let mut count = 0;
   for group in profile.tweets.content {
      for tweet in group {
         if count == 8 {
            break;
         }
         out.push_str(&format_tweet(&origin, &tweet));
         out.push_str("\n\n");
         count += 1;
      }
   }
   Ok(out)
}

async fn post_text(state: &AppState, id: &str) -> Result<String, String> {
   let tweet = state.api.get_tweet(id).await.map_err(brief)?;
   Ok(format_tweet(&origin(state), &tweet))
}

fn format_tweet(origin: &str, tweet: &Tweet) -> String {
   let tweet = tweet.retweet.as_deref().unwrap_or(tweet);
   let mut out = format!(
      "@{} ({})\n{}\n{}/{}/status/{}",
      tweet.user.username,
      tweet.user.fullname,
      tweet.text.trim(),
      origin,
      tweet.user.username,
      tweet.id
   );
   if let Some(quote) = &tweet.quote {
      out.push_str(&format!(
         "\nQuoted @{}: {}",
         quote.user.username,
         quote.text.trim()
      ));
   }
   out
}

fn brief(err: Error) -> String {
   let text = err.to_string();
   text.chars().take(400).collect()
}

fn skill_digest() -> String {
   format!("sha256:{}", hex(&sha256(SKILL_MD.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
   const ALPHABET: &[u8; 16] = b"0123456789abcdef";
   let mut out = String::with_capacity(bytes.len() * 2);
   for byte in bytes {
      out.push(ALPHABET[(byte >> 4) as usize] as char);
      out.push(ALPHABET[(byte & 0xf) as usize] as char);
   }
   out
}

fn sha256(message: &[u8]) -> [u8; 32] {
   const K: [u32; 64] = [
      0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
      0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
      0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
      0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
      0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
      0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
      0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
      0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
      0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
      0xc67178f2,
   ];
   let mut hash: [u32; 8] = [
      0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
      0x5be0cd19,
   ];
   let bit_len = (message.len() as u64).saturating_mul(8);
   let mut padded = message.to_vec();
   padded.push(0x80);
   while padded.len() % 64 != 56 {
      padded.push(0);
   }
   padded.extend_from_slice(&bit_len.to_be_bytes());
   for chunk in padded.chunks(64) {
      let mut words = [0_u32; 64];
      for (index, block) in chunk.chunks(4).take(16).enumerate() {
         words[index] = u32::from_be_bytes(block.try_into().unwrap_or([0; 4]));
      }
      for index in 16..64 {
         let small = words[index - 15].rotate_right(7)
            ^ words[index - 15].rotate_right(18)
            ^ (words[index - 15] >> 3);
         let big = words[index - 2].rotate_right(17)
            ^ words[index - 2].rotate_right(19)
            ^ (words[index - 2] >> 10);
         words[index] = words[index - 16]
            .wrapping_add(small)
            .wrapping_add(words[index - 7])
            .wrapping_add(big);
      }
      let mut a = hash[0];
      let mut b = hash[1];
      let mut c = hash[2];
      let mut d = hash[3];
      let mut e = hash[4];
      let mut f = hash[5];
      let mut g = hash[6];
      let mut h = hash[7];
      for index in 0..64 {
         let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
         let choose = (e & f) ^ ((!e) & g);
         let temp1 = h
            .wrapping_add(sum1)
            .wrapping_add(choose)
            .wrapping_add(K[index])
            .wrapping_add(words[index]);
         let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
         let majority = (a & b) ^ (a & c) ^ (b & c);
         let temp2 = sum0.wrapping_add(majority);
         h = g;
         g = f;
         f = e;
         e = d.wrapping_add(temp1);
         d = c;
         c = b;
         b = a;
         a = temp1.wrapping_add(temp2);
      }
      hash[0] = hash[0].wrapping_add(a);
      hash[1] = hash[1].wrapping_add(b);
      hash[2] = hash[2].wrapping_add(c);
      hash[3] = hash[3].wrapping_add(d);
      hash[4] = hash[4].wrapping_add(e);
      hash[5] = hash[5].wrapping_add(f);
      hash[6] = hash[6].wrapping_add(g);
      hash[7] = hash[7].wrapping_add(h);
   }
   let mut out = [0_u8; 32];
   for (index, word) in hash.iter().enumerate() {
      out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
   }
   out
}

#[cfg(test)]
mod tests {
   use super::*;
   use axum::http::HeaderMap;

   #[test]
   fn sha256_matches_the_empty_and_abc_vectors() {
      assert_eq!(
         hex(&sha256(b"")),
         "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
      );
      assert_eq!(
         hex(&sha256(b"abc")),
         "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
      );
   }

   #[test]
   fn markdown_accept_beats_html_only_when_preferred() {
      let mut headers = HeaderMap::new();
      headers.insert(header::ACCEPT, HeaderValue::from_static("text/markdown"));
      assert!(wants_markdown(&headers));
      headers.insert(
         header::ACCEPT,
         HeaderValue::from_static("text/html,application/xhtml+xml,*/*;q=0.8"),
      );
      assert!(!wants_markdown(&headers));
      headers.insert(
         header::ACCEPT,
         HeaderValue::from_static("text/markdown, text/html;q=0.5"),
      );
      assert!(wants_markdown(&headers));
   }

   #[test]
   fn html_keeps_the_heading_and_the_link() {
      let markdown = html_to_markdown(
         "<html><h1>Hello</h1><p>See <a href=\"/github\">this</a> post.</p><script>nope</script></html>",
      );
      assert!(markdown.contains("# Hello"), "{markdown}");
      assert!(markdown.contains("[this](/github)"), "{markdown}");
      assert!(!markdown.contains("nope"), "{markdown}");
   }

   #[test]
   fn index_md_points_at_the_page() {
      assert_eq!(strip_index_md("/index.md").as_deref(), Some("/"));
      assert_eq!(
         strip_index_md("/github/status/1/index.md").as_deref(),
         Some("/github/status/1")
      );
      assert_eq!(strip_index_md("/.well-known/agent-skills/read-teapawt/SKILL.md"), None);
   }
}
