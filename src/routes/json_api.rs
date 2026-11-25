//! JSON for search-pass holders.
//!
//! Bodies take the useful parts of X's recent search and of FxTwitter: a
//! `data` / `meta` envelope, string ids, and field selection so a client can
//! skip bios, cards, and video variants. Authors are embedded, because a
//! second lookup costs more than a short name. Empty fields are left out.
//!
//! A call is admitted by the same check as the search page: a paid pass
//! cookie, a whitelisted address, or an API key. The key is what lets a
//! script skip the checkbox.
//!
//! A script reads the body, not the page, so every refusal is JSON in one
//! shape, [`Refusal`], including the ones raised before a handler runs. The
//! pass, the query string and the path are read by extractors that refuse
//! that way, and the id guard and the Anti-DDoS check ask [`owns`] before
//! they answer one of these paths with HTML.

use std::{
   any::Any,
   borrow::Cow,
   future::Future,
   slice,
};

use axum::{
   Router,
   extract::{
      FromRequestParts,
      Path,
      Query,
      State,
   },
   http::{
      HeaderValue,
      StatusCode,
      header,
      request::Parts,
   },
   response::{
      Html,
      IntoResponse,
      Response,
   },
   routing::get,
};
use axum_extra::extract::CookieJar;
use serde::{
   Deserialize,
   Serialize,
   de::DeserializeOwned,
};

use super::{
   admin_stats,
   helpers,
   search::SearchQuery,
};
use crate::{
   AppState,
   cache::{
      Hit,
      keys as cache_keys,
      ttl,
   },
   error::{
      Error,
      Result,
   },
   types::{
      self,
      Card,
      CardKind,
      EntityKind,
      List,
      PaginatedResult,
      Poll,
      Timeline,
      Tweet,
      User,
      VerifiedType,
      Video,
      VideoType,
   },
   utils::formatters::direct_media_url,
   views::{
      self,
      layout::PageLayout,
   },
};

const MAX_QUERY: usize = 512;
const MAX_CURSOR: usize = 4096;
const MAX_LIST: usize = 200;
/// Posts one `/api/tweets?ids=` call may name.
const MAX_BATCH: usize = 100;
/// Fresh and stale seconds for a stored page that is not a search.
const PAGE_TTL: (u64, u64) = (ttl::DEFAULT, ttl::DEFAULT_STALE);

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/api", get(docs))
      .route("/api/openapi.json", get(openapi_doc))
      .route("/api/search", get(search))
      .route("/api/lists/{id}/tweets", get(list_tweets))
      .route("/api/communities/{id}/tweets", get(community_tweets))
      .route("/api/users/id/{id}", get(user_by_id))
      .route("/api/users/{username}", get(user))
      .route("/api/users/{username}/tweets", get(user_tweets))
      .route("/api/users/{username}/with_replies", get(user_replies))
      .route("/api/users/{username}/about", get(about))
      .route("/api/users/{username}/following/ids", get(following_ids))
      .route("/api/users/{username}/following", get(following))
      .route("/api/users/{username}/followers", get(followers))
      .route("/api/users/{username}/friends", get(friends))
      .route("/api/tweets/{id}/retweeters", get(retweeters))
      .route("/api/tweets/{id}", get(tweet))
      .route("/api/tweets", get(tweets))
}

/// Whether `path` is one of the routes above. A route added to [`router`] is
/// added here.
///
/// Exact shapes, not the `/api` prefix: `/api/v1/statuses/{id}` is the embed
/// endpoint, and `/api/lists/{slug}` and `/api/search/rss` are the pages of
/// an account named `api`. Those call X without asking for a pass.
pub(super) fn owns(path: &str) -> bool {
   let Some(rest) = path.strip_prefix("/api") else {
      return false;
   };
   if rest.is_empty() {
      return true;
   }
   let Some(rest) = rest.strip_prefix('/') else {
      return false;
   };
   let segments: Vec<&str> = rest.split('/').collect();
   matches!(
      segments.as_slice(),
      ["openapi.json" | "search" | "tweets"]
         | ["lists" | "communities", _, "tweets"]
         | ["users", _]
         | ["users", "id", _]
         | [
            "users",
            _,
            "tweets" | "with_replies" | "about" | "following" | "followers" | "friends"
         ]
         | ["users", _, "following", "ids"]
         | ["tweets", _]
         | ["tweets", _, "retweeters"]
   )
}

/// The id guard's refusal for one of these paths, in this API's shape.
pub(super) fn invalid_id() -> Response {
   Refusal::invalid("id is not a numeric id.").into_response()
}

/// A value, or the refusal that explains why there is none.
type Checked<T> = std::result::Result<T, Refusal>;

/// What a handler answers.
type Reply = Checked<Response>;

/// A call that returned no results, in the one shape every failure has.
#[derive(Debug)]
struct Refusal {
   status:      StatusCode,
   code:        &'static str,
   message:     Cow<'static, str>,
   /// Seconds to wait, sent as `Retry-After` with a 429.
   retry_after: Option<u64>,
}

impl Refusal {
   fn invalid(message: impl Into<Cow<'static, str>>) -> Self {
      Self {
         status:      StatusCode::BAD_REQUEST,
         code:        "invalid",
         message:     message.into(),
         retry_after: None,
      }
   }

   /// No paid pass on this request. A key that was sent and not accepted is
   /// named, so its owner does not go looking for a header that is there.
   fn pass_required(sent_key: bool) -> Self {
      let message = if sent_key {
         "That API key was not accepted. It is mistyped or revoked, or its search pass has run \
          out. Keys are made on /search-pass."
      } else {
         "A search pass is required. Create an API key at /search-pass, sign in, or whitelist this \
          address."
      };
      Self {
         status:      StatusCode::UNAUTHORIZED,
         code:        "pass_required",
         message:     Cow::Borrowed(message),
         retry_after: None,
      }
   }
}

impl From<Error> for Refusal {
   fn from(err: Error) -> Self {
      let (status, _, message) = err.presentation();
      let code = match &err {
         Error::NotFound(_) | Error::UserNotFound(_) | Error::TweetNotFound(_) => "not_found",
         Error::UserSuspended(_) => "suspended",
         Error::ProtectedUser(_) => "protected",
         Error::SearchDenylisted => "blocked",
         Error::InvalidUrl(_) => "invalid",
         Error::RateLimited | Error::IpThrottled | Error::ClientBudgetExhausted => "rate_limited",
         Error::TransientUpstream
         | Error::SearchRefused
         | Error::SearchTimelineFailed
         | Error::NoSessions
         | Error::SessionRejected(_) => "unavailable",
         _ => "error",
      };
      // The same lines the HTML error page writes, so a failing API call is
      // as visible in the log as a failing page.
      if matches!(
         err,
         Error::TransientUpstream | Error::SearchRefused | Error::SearchTimelineFailed
      ) {
         tracing::trace!(error = ?err, "upstream empty");
      } else if status.is_server_error() {
         tracing::error!(error = ?err, "api request failed");
      } else if status == StatusCode::TOO_MANY_REQUESTS {
         tracing::debug!(error = ?err, "api request refused");
      }
      Self {
         status,
         code,
         message: Cow::Owned(message.to_owned()),
         retry_after: err.retry_after(),
      }
   }
}

impl IntoResponse for Refusal {
   fn into_response(self) -> Response {
      let mut response = json_body(self.status, &Failure {
         errors: [FailureItem {
            code:    self.code,
            message: &self.message,
         }],
      });
      let headers = response.headers_mut();
      if self.status == StatusCode::UNAUTHORIZED {
         // A 401 names the scheme that would have been accepted.
         headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
      }
      if let Some(seconds) = self.retry_after {
         headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
      }
      response
   }
}

#[derive(Serialize)]
struct Failure<'a> {
   errors: [FailureItem<'a>; 1],
}

#[derive(Serialize)]
struct FailureItem<'a> {
   code:    &'static str,
   message: &'a str,
}

/// The search pass, checked before anything else is read from the request.
///
/// First in every handler that returns data, so a call without a pass is
/// told that and nothing about its parameters.
struct Pass;

impl FromRequestParts<AppState> for Pass {
   type Rejection = Refusal;

   async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Checked<Self> {
      if state
         .passes
         .admits(&CookieJar::from_headers(&parts.headers))
      {
         return Ok(Self);
      }
      let sent_key = parts.headers.contains_key(header::AUTHORIZATION)
         || parts.headers.contains_key("x-api-key");
      Err(Refusal::pass_required(sent_key))
   }
}

/// Query parameters. A string that cannot be read, such as a repeated name
/// or a `count` that is not a number, is refused in this API's shape.
struct ApiQuery<T>(T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
   S: Send + Sync,
   T: DeserializeOwned,
{
   type Rejection = Refusal;

   async fn from_request_parts(parts: &mut Parts, _state: &S) -> Checked<Self> {
      Query::try_from_uri(&parts.uri)
         .map(|Query(value)| Self(value))
         .map_err(|err| Refusal::invalid(err.body_text()))
   }
}

/// The one `{id}` or `{username}` segment of a path.
struct Segment(String);

impl<S> FromRequestParts<S> for Segment
where
   S: Send + Sync,
{
   type Rejection = Refusal;

   async fn from_request_parts(parts: &mut Parts, state: &S) -> Checked<Self> {
      Path::<String>::from_request_parts(parts, state)
         .await
         .map(|Path(value)| Self(value))
         .map_err(|_| Refusal::invalid("That path is not usable."))
   }
}

#[derive(Deserialize)]
struct SearchParams {
   #[serde(flatten)]
   search:       SearchQuery,
   #[serde(default, rename = "tweet.fields")]
   tweet_fields: Option<String>,
   #[serde(default, rename = "user.fields")]
   user_fields:  Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PageParams {
   cursor:       Option<String>,
   #[serde(default, rename = "tweet.fields")]
   tweet_fields: Option<String>,
   #[serde(default, rename = "user.fields")]
   user_fields:  Option<String>,
   ranking:      Option<String>,
   count:        Option<u32>,
   ids:          Option<String>,
}

impl PageParams {
   fn cursor(&self) -> Checked<Option<&str>> {
      cursor(self.cursor.as_deref())
   }

   /// Fields for a list of posts. An author is its id and two names unless
   /// `user.fields` asks for more.
   fn post_fields(&self) -> Checked<(TweetFields, UserFields)> {
      Ok((
         TweetFields::parse(self.tweet_fields.as_deref())?,
         UserFields::parse(self.user_fields.as_deref(), UserFields::stub())?,
      ))
   }

   /// Fields for a list of accounts. The account is the result there, so it
   /// starts from the same set user search returns.
   fn account_fields(&self) -> Checked<UserFields> {
      UserFields::parse(self.user_fields.as_deref(), UserFields::directory())
   }
}

async fn openapi_doc(State(state): State<AppState>) -> Response {
   let origin = crate::utils::host::current_origin(state.config.url_prefix());
   let body = super::json_openapi::document(&origin);
   (
      [
         (
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
         ),
         (
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=300"),
         ),
      ],
      body.to_string(),
   )
      .into_response()
}

async fn docs(State(state): State<AppState>, jar: CookieJar) -> Response {
   let prefs = types::Prefs::from_cookies(&jar, &state.config);
   let origin = crate::utils::host::current_origin(state.config.url_prefix());
   let content = views::json_api::render(&origin, state.passes.admits(&jar));
   let markup = PageLayout::new(&state.config, "API", content)
      .description("JSON API for search, profiles, and posts. A search pass is required.")
      .prefs(&prefs)
      .referer("/api")
      .render();
   let mut response = Html(markup.into_string()).into_response();
   response.headers_mut().insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("private, no-store"),
   );
   response
}

/// The result tab `f` names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
   Latest,
   Top,
   Media,
   Users,
   Lists,
}

impl Tab {
   fn parse(filter: Option<&str>) -> Checked<Self> {
      match filter.map(str::trim) {
         None | Some("" | "latest" | "tweets" | "live") => Ok(Self::Latest),
         Some("top") => Ok(Self::Top),
         Some("media") => Ok(Self::Media),
         Some("users") => Ok(Self::Users),
         Some("lists") => Ok(Self::Lists),
         Some(_) => {
            Err(Refusal::invalid(
               "f must be latest, top, media, users, or lists.",
            ))
         },
      }
   }

   /// The `product` X's `SearchTimeline` is asked for.
   const fn product(self) -> &'static str {
      match self {
         Self::Top => "Top",
         Self::Media => "Media",
         Self::Latest | Self::Users | Self::Lists => "Latest",
      }
   }
}

async fn search(
   _: Pass,
   State(state): State<AppState>,
   ApiQuery(params): ApiQuery<SearchParams>,
) -> Reply {
   let query = &params.search;
   if let Some(name) = unknown_named_filter(query) {
      return Err(Refusal::invalid(format!("Unknown filter \"{name}\".")));
   }
   let tab = Tab::parse(query.filter.as_deref())?;
   let tweets = TweetFields::parse(params.tweet_fields.as_deref())?;
   let users = UserFields::parse(
      params.user_fields.as_deref(),
      if tab == Tab::Users {
         UserFields::directory()
      } else {
         UserFields::stub()
      },
   )?;
   check_query(query)?;
   let after = cursor(query.cursor.as_deref())?;

   match tab {
      Tab::Users => {
         let text = search_text(query)?;
         let key = cache_keys::search_users(text, after);
         let fetch = {
            let (text, after) = (text.to_owned(), after.map(str::to_owned));
            move |state: AppState| {
               let (text, after) = (text.clone(), after.clone());
               async move { state.api.search_users(&text, after.as_deref()).await }
            }
         };
         let found = search_page(&state, &key, after.is_some(), fetch).await?;
         let empty = found.content.is_empty();
         Ok(admin_stats::search_result(
            accounts_body(&state, found, &users, after),
            empty,
         ))
      },
      Tab::Lists => {
         let text = search_text(query)?;
         let key = cache_keys::search_lists(text, after);
         let fetch = {
            let (text, after) = (text.to_owned(), after.map(str::to_owned));
            move |state: AppState| {
               let (text, after) = (text.clone(), after.clone());
               async move { state.api.search_lists(&text, after.as_deref()).await }
            }
         };
         let found = search_page(&state, &key, after.is_some(), fetch).await?;
         let empty = found.content.is_empty();
         // As with accounts, a page of no lists is the end of them.
         let next = found.bottom.clone().filter(|_| !empty);
         let body = Page::new(found.content.iter().map(list_row).collect(), next, after);
         Ok(admin_stats::search_result(json_ok(&body), empty))
      },
      Tab::Latest | Tab::Top | Tab::Media => {
         let built = query.to_query().build();
         if built.trim().is_empty() {
            return Err(Refusal::invalid("q is required."));
         }
         if built.chars().count() > MAX_QUERY.saturating_mul(2) {
            return Err(Refusal::invalid("That search is too long."));
         }
         let product = tab.product();
         let key = cache_keys::search_timeline(&built, product, after);
         let fetch = {
            let (built, after) = (built.clone(), after.map(str::to_owned));
            move |state: AppState| {
               let (built, after) = (built.clone(), after.clone());
               async move { state.api.search(&built, after.as_deref(), product).await }
            }
         };
         let timeline = search_page(&state, &key, after.is_some(), fetch).await?;
         let next = timeline.bottom.clone();
         let mut posts =
            super::search::dedup_search_tweets(timeline.content.into_iter().flatten().collect());
         if tab == Tab::Media {
            posts.retain(Tweet::has_gallery_media);
         }
         let empty = posts.is_empty();
         helpers::remember_tweets(&state, &posts);
         let body = Page::new(
            posts
               .iter()
               .map(|tweet| post(tweet, &tweets, &users, false))
               .collect(),
            next,
            after,
         )
         .with_query(built);
         Ok(admin_stats::search_result(json_ok(&body), empty))
      },
   }
}

async fn user(_: Pass, State(state): State<AppState>, Segment(username): Segment) -> Reply {
   let name = handle(&username)?;
   let user = helpers::get_cached_user(&state, name).await?;
   Ok(json_ok(&One {
      data: account(&user, &UserFields::full()),
   }))
}

async fn user_by_id(_: Pass, State(state): State<AppState>, Segment(id): Segment) -> Reply {
   let id = numeric_id(&id, "a user")?;
   let user = state.api.get_user_by_id(id).await?;
   helpers::remember_user_id(&state, &user);
   Ok(json_ok(&One {
      data: account(&user, &UserFields::full()),
   }))
}

async fn user_tweets(
   _: Pass,
   State(state): State<AppState>,
   Segment(username): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   let (tweets, users) = params.post_fields()?;
   let after = params.cursor()?;
   let (name, user_id) = timeline_owner(&state, &username, after.is_some()).await?;
   let key = cache_keys::filtered_posts(name, "all", "recent", after);
   let timeline = cached(
      &state,
      &key,
      PAGE_TTL,
      state.api.get_filtered_posts(&user_id, after, "all", false),
   )
   .await?;
   Ok(posts_body(&state, timeline, &tweets, &users, after))
}

async fn user_replies(
   _: Pass,
   State(state): State<AppState>,
   Segment(username): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   let (tweets, users) = params.post_fields()?;
   let after = params.cursor()?;
   let (_, user_id) = timeline_owner(&state, &username, after.is_some()).await?;
   let key = cache_keys::user_replies(&user_id, after);
   let timeline = cached(
      &state,
      &key,
      PAGE_TTL,
      state.api.get_user_tweets_and_replies(&user_id, after),
   )
   .await?;
   Ok(posts_body(&state, timeline, &tweets, &users, after))
}

async fn about(_: Pass, State(state): State<AppState>, Segment(username): Segment) -> Reply {
   let name = handle(&username)?;
   let key = cache_keys::account_context(name);
   let context = cached(
      &state,
      &key,
      (ttl::ACCOUNT_CONTEXT, 0),
      state.api.get_account_context(name),
   )
   .await?;
   Ok(json_ok(&One {
      data: About {
         account_based_in:  &context.account_based_in,
         connection_source: &context.connection_source,
         location_accurate: context.location_accurate,
      },
   }))
}

#[derive(Serialize)]
struct About<'a> {
   #[serde(skip_serializing_if = "empty_ref")]
   account_based_in:  &'a str,
   #[serde(skip_serializing_if = "empty_ref")]
   connection_source: &'a str,
   #[serde(skip_serializing_if = "Option::is_none")]
   location_accurate: Option<bool>,
}

async fn tweet(
   _: Pass,
   State(state): State<AppState>,
   Segment(id): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   let id = numeric_id(&id, "a post")?;
   let (tweets, users) = params.post_fields()?;
   let tweet = helpers::get_cached_tweet(&state, id).await?;
   helpers::remember_tweets(&state, slice::from_ref(&tweet));
   Ok(json_ok(&One {
      data: post(&tweet, &tweets, &users, false),
   }))
}

async fn tweets(
   _: Pass,
   State(state): State<AppState>,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   let (tweets, users) = params.post_fields()?;
   let ids = batch_ids(params.ids.as_deref())?;
   let body = state.api.tweets_by_ids(&ids).await?;
   let posts = crate::api::parse_tweets_by_ids(&body, &ids);
   helpers::remember_tweets(&state, &posts);
   Ok(json_ok(&Page::new(
      posts
         .iter()
         .map(|tweet| post(tweet, &tweets, &users, false))
         .collect(),
      None,
      None,
   )))
}

async fn list_tweets(
   _: Pass,
   State(state): State<AppState>,
   Segment(id): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   let id = numeric_id(&id, "a list")?;
   let (tweets, users) = params.post_fields()?;
   let after = params.cursor()?;
   let key = cache_keys::list_tweets(id, after);
   let timeline = cached(&state, &key, PAGE_TTL, state.api.get_list_tweets(id, after)).await?;
   Ok(posts_body(&state, timeline, &tweets, &users, after))
}

async fn community_tweets(
   _: Pass,
   State(state): State<AppState>,
   Segment(id): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   let id = numeric_id(&id, "a community")?;
   let ranking = match params.ranking.as_deref().map(str::trim) {
      None | Some("" | "latest" | "recency") => "Recency",
      Some("top" | "relevance") => "Relevance",
      Some(_) => return Err(Refusal::invalid("ranking must be latest or top.")),
   };
   let (tweets, users) = params.post_fields()?;
   let after = params.cursor()?;
   let key = cache_keys::community_tweets(id, ranking, after);
   let timeline = cached(
      &state,
      &key,
      PAGE_TTL,
      state.api.get_community_tweets(id, after, ranking),
   )
   .await?;
   Ok(posts_body(&state, timeline, &tweets, &users, after))
}

async fn retweeters(
   _: Pass,
   State(state): State<AppState>,
   Segment(id): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   let id = numeric_id(&id, "a post")?;
   let fields = params.account_fields()?;
   let after = params.cursor()?;
   let key = cache_keys::retweeters(id, after);
   let found = cached(&state, &key, PAGE_TTL, state.api.get_retweeters(id, after)).await?;
   Ok(accounts_body(&state, found, &fields, after))
}

async fn following(
   _: Pass,
   State(state): State<AppState>,
   Segment(username): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   user_graph(&state, &username, &params, Graph::Following).await
}

async fn following_ids(
   _: Pass,
   State(state): State<AppState>,
   Segment(username): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   user_graph(&state, &username, &params, Graph::Ids).await
}

async fn followers(
   _: Pass,
   State(state): State<AppState>,
   Segment(username): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   user_graph(&state, &username, &params, Graph::Followers).await
}

async fn friends(
   _: Pass,
   State(state): State<AppState>,
   Segment(username): Segment,
   ApiQuery(params): ApiQuery<PageParams>,
) -> Reply {
   user_graph(&state, &username, &params, Graph::Friends).await
}

/// Which list of accounts around one account is wanted, and the X call
/// behind it.
#[derive(Clone, Copy)]
enum Graph {
   /// The web client's `Following` query.
   Following,
   /// `1.1/friends/ids`.
   Ids,
   /// `1.1/followers/list`.
   Followers,
   /// `1.1/friends/list`.
   Friends,
}

impl Graph {
   const fn label(self) -> &'static str {
      match self {
         Self::Following => "following",
         Self::Ids => "ids",
         Self::Followers => "followers",
         Self::Friends => "friends",
      }
   }

   /// The page size to ask X for. `Following` has no such parameter.
   fn count(self, raw: Option<u32>) -> Checked<u32> {
      match self {
         Self::Ids => bounded_count(raw, 5_000, 5_000),
         Self::Followers | Self::Friends => bounded_count(raw, 20, 200),
         Self::Following => Ok(20),
      }
   }

   /// Whether X answers 401 for this list when the account is protected.
   ///
   /// The 1.1 calls do, and a 401 is what takes one of this instance's
   /// accounts out of rotation for good. The client tells that refusal from a
   /// dead token by its body, but these calls do not lean on that alone: the
   /// profile is read before every page, so a protected account is refused
   /// here and X is never asked.
   const fn refuses_protected_with_401(self) -> bool {
      matches!(self, Self::Ids | Self::Followers | Self::Friends)
   }
}

async fn user_graph(state: &AppState, username: &str, params: &PageParams, kind: Graph) -> Reply {
   let fields = params.account_fields()?;
   let after = params.cursor()?;
   let count = kind.count(params.count)?;
   let later_page = after.is_some() && !kind.refuses_protected_with_401();
   let (_, user_id) = timeline_owner(state, username, later_page).await?;
   let key = cache_keys::user_graph(kind.label(), &user_id, count, after);
   let found = match kind {
      Graph::Ids => {
         let (ids, next) = cached(
            state,
            &key,
            PAGE_TTL,
            state.api.friend_ids(&user_id, after, count),
         )
         .await?;
         return Ok(json_ok(&Page::new(ids, next, after)));
      },
      Graph::Following => {
         cached(
            state,
            &key,
            PAGE_TTL,
            state.api.get_following(&user_id, after),
         )
         .await?
      },
      Graph::Followers => {
         cached(
            state,
            &key,
            PAGE_TTL,
            state.api.followers_list(&user_id, after, count),
         )
         .await?
      },
      Graph::Friends => {
         cached(
            state,
            &key,
            PAGE_TTL,
            state.api.friends_list(&user_id, after, count),
         )
         .await?
      },
   };
   Ok(accounts_body(state, found, &fields, after))
}

/// The handle in a `{username}` path and the numeric id X's calls are keyed
/// by.
///
/// A first page asks X for the profile, so a suspended or protected account
/// is refused by name instead of answering as an empty list. A later page
/// has passed that check already and only needs the id, which the directory
/// holds from that first call. Asking again spent a `UserByScreenName` on
/// every page of a walk through one account.
async fn timeline_owner<'a>(
   state: &AppState,
   raw: &'a str,
   later_page: bool,
) -> Checked<(&'a str, String)> {
   let name = handle(raw)?;
   if later_page {
      return Ok((name, helpers::user_id(state, name).await?));
   }
   let user = helpers::get_cached_user(state, name).await?;
   if user.id.is_empty() {
      return Err(Error::UserNotFound(name.to_owned()).into());
   }
   if user.suspended {
      return Err(Error::UserSuspended(user.username).into());
   }
   if user.protected {
      return Err(Error::ProtectedUser(user.username).into());
   }
   Ok((name, user.id))
}

/// A page of accounts.
fn accounts_body(
   state: &AppState,
   found: PaginatedResult<User>,
   fields: &UserFields,
   sent: Option<&str>,
) -> Response {
   for user in &found.content {
      helpers::remember_user_id(state, user);
   }
   let next = accounts_cursor(found.bottom, found.content.len());
   json_ok(&Page::new(
      found
         .content
         .iter()
         .map(|user| account(user, fields))
         .collect(),
      next,
      sent,
   ))
}

/// The cursor after a page of `count` accounts, when it leads anywhere.
///
/// X ends these lists with a page of no accounts, or with a `0|…` cursor on
/// the last page that has some. Either one only leads to an empty page, so
/// it is left out and the client is not sent to fetch it.
fn accounts_cursor(next: Option<String>, count: usize) -> Option<String> {
   next.filter(|next| count > 0 && !next.starts_with("0|"))
}

/// A page of posts. An empty one keeps its cursor: X filters whole pages out
/// of a timeline that still has more behind them.
fn posts_body(
   state: &AppState,
   timeline: Timeline,
   tweets: &TweetFields,
   users: &UserFields,
   sent: Option<&str>,
) -> Response {
   let next = timeline.bottom.clone();
   let posts = flatten_posts(timeline);
   helpers::remember_tweets(state, &posts);
   json_ok(&Page::new(
      posts
         .iter()
         .map(|tweet| post(tweet, tweets, users, false))
         .collect(),
      next,
      sent,
   ))
}

fn bounded_count(raw: Option<u32>, default: u32, max: u32) -> Checked<u32> {
   let Some(count) = raw else {
      return Ok(default);
   };
   if (1..=max).contains(&count) {
      Ok(count)
   } else {
      Err(Refusal::invalid(format!("count must be from 1 to {max}.")))
   }
}

/// The `{id}` of a path, as the digits X's ids are. `what` names it in the
/// refusal, article included: "a list".
fn numeric_id<'a>(id: &'a str, what: &str) -> Checked<&'a str> {
   if (1..=19).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit()) {
      Ok(id)
   } else {
      Err(Refusal::invalid(format!("id is not {what} id.")))
   }
}

/// The post ids in `ids=`, each once and in the order given, written the way
/// X writes them so the answer can be matched back to the request.
fn batch_ids(raw: Option<&str>) -> Checked<Vec<String>> {
   let raw = raw.map(str::trim).unwrap_or_default();
   if raw.is_empty() {
      return Err(Refusal::invalid("ids is required."));
   }
   let mut ids = Vec::new();
   for part in raw.split(|ch: char| ch == ',' || ch.is_whitespace()) {
      if part.is_empty() {
         continue;
      }
      let id = numeric_id(part, "a post")
         .ok()
         .and_then(|digits| digits.parse::<i64>().ok())
         .filter(|id| *id > 0)
         .ok_or_else(|| Refusal::invalid("ids must be post ids."))?
         .to_string();
      if !ids.contains(&id) {
         ids.push(id);
      }
   }
   if ids.is_empty() || ids.len() > MAX_BATCH {
      return Err(Refusal::invalid(format!(
         "ids needs between 1 and {MAX_BATCH} post ids."
      )));
   }
   Ok(ids)
}

/// A page stored under `key`, or the one `fetch` returns, stored for the
/// next call. `ttl` is its fresh and stale seconds.
async fn cached<T, Fut>(state: &AppState, key: &str, ttl: (u64, u64), fetch: Fut) -> Result<T>
where
   T: Any + Clone + Send + Sync,
   Fut: Future<Output = Result<T>>,
{
   if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) = state.cache.lookup(key) {
      return Ok(hit);
   }
   let result = fetch.await;
   if let Ok(ref data) = result {
      state.cache.set_swr(key, data, ttl.0, ttl.1);
   }
   result
}

/// One page of a search, under the key the HTML search stores it by.
///
/// A first page past its fresh time is answered from the store while one
/// refresh runs. A later page belongs to its cursor and is kept as it is.
async fn search_page<T, F, Fut>(
   state: &AppState,
   key: &str,
   later_page: bool,
   fetch: F,
) -> Result<T>
where
   T: Any + Clone + Send + Sync,
   F: Fn(AppState) -> Fut + Clone + Send + 'static,
   Fut: Future<Output = Result<T>> + Send + 'static,
{
   if later_page {
      let ttl = (ttl::SEARCH_CURSOR, ttl::SEARCH_CURSOR_STALE);
      return cached(state, key, ttl, fetch(state.clone())).await;
   }
   let refresh = {
      let key = key.to_owned();
      let fetch = fetch.clone();
      move |state: AppState| {
         async move {
            if let Ok(data) = fetch(state.clone()).await {
               state
                  .cache
                  .set_swr(&key, &data, ttl::SEARCH, ttl::SEARCH_STALE);
            }
         }
      }
   };
   if let Some(hit) = helpers::swr_take(state, key, refresh) {
      return Ok(hit);
   }
   let result = fetch(state.clone()).await;
   if let Ok(ref data) = result {
      state
         .cache
         .set_swr(key, data, ttl::SEARCH, ttl::SEARCH_STALE);
   }
   result
}

fn check_query(params: &SearchQuery) -> Checked<()> {
   let query = params.query.as_deref().unwrap_or("").trim();
   if query.chars().count() > MAX_QUERY {
      return Err(Refusal::invalid("q is longer than 512 characters."));
   }
   if query.bytes().any(|byte| byte.is_ascii_control()) {
      return Err(Refusal::invalid("q is not a usable query."));
   }
   for (name, value, limit) in [
      ("from", params.from.as_deref(), 200),
      ("filters", params.filters.as_deref(), MAX_LIST),
      ("exclude", params.exclude.as_deref(), MAX_LIST),
   ] {
      if value.is_some_and(|value| {
         value.len() > limit || value.bytes().any(|byte| byte.is_ascii_control())
      }) {
         return Err(Refusal::invalid(format!("{name} is not a usable filter.")));
      }
   }
   // These three are appended to the search as written, so anything but the
   // documented form would reach X as part of the query text. Sent empty,
   // they are left out, as the search form sends them.
   for (name, value) in [("since", &params.since), ("until", &params.until)] {
      if value
         .as_deref()
         .is_some_and(|date| !date.is_empty() && !is_date(date))
      {
         return Err(Refusal::invalid(format!("{name} must be YYYY-MM-DD.")));
      }
   }
   if params
      .min_faves
      .as_deref()
      .is_some_and(|count| count.len() > 12 || !count.bytes().all(|byte| byte.is_ascii_digit()))
   {
      return Err(Refusal::invalid("min_faves must be a whole number."));
   }
   Ok(())
}

/// `YYYY-MM-DD`, the form `since:` and `until:` take inside a query.
fn is_date(value: &str) -> bool {
   let bytes = value.as_bytes();
   bytes.len() == 10
      && bytes.iter().enumerate().all(|(index, byte)| {
         if matches!(index, 4 | 7) {
            *byte == b'-'
         } else {
            byte.is_ascii_digit()
         }
      })
}

/// The text a people or list search runs on. Those two take no filters.
fn search_text(params: &SearchQuery) -> Checked<&str> {
   let text = params.query.as_deref().unwrap_or("").trim();
   if text.is_empty() {
      return Err(Refusal::invalid("q is required."));
   }
   Ok(text)
}

fn unknown_named_filter(params: &SearchQuery) -> Option<String> {
   for raw in [params.filters.as_deref(), params.exclude.as_deref()]
      .into_iter()
      .flatten()
   {
      for name in raw.split(|ch: char| ch == ',' || ch.is_whitespace()) {
         let name = name.trim();
         if !name.is_empty() && !types::VALID_FILTERS.contains(&name) {
            return Some(name.to_owned());
         }
      }
   }
   None
}

fn cursor(value: Option<&str>) -> Checked<Option<&str>> {
   let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
      return Ok(None);
   };
   if value.len() > MAX_CURSOR || value.bytes().any(|byte| byte.is_ascii_control()) {
      return Err(Refusal::invalid(
         "cursor is not a page token from a previous response.",
      ));
   }
   Ok(Some(value))
}

fn handle(raw: &str) -> Checked<&str> {
   let name = raw.trim().trim_start_matches('@');
   if (1..=15).contains(&name.len())
      && name
         .chars()
         .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
   {
      Ok(name)
   } else {
      Err(Refusal::invalid("username is not a valid X handle."))
   }
}

fn json_ok(body: &impl Serialize) -> Response {
   json_body(StatusCode::OK, body)
}

/// A JSON answer no cache may keep. It was produced for one pass, and the
/// same URL without that pass is a refusal.
fn json_body(status: StatusCode, body: &impl Serialize) -> Response {
   let bytes = serde_json::to_vec(body).unwrap_or_else(|_| {
      br#"{"errors":[{"code":"error","message":"Could not encode the response."}]}"#.to_vec()
   });
   let mut response = (status, bytes).into_response();
   let headers = response.headers_mut();
   headers.insert(
      header::CONTENT_TYPE,
      HeaderValue::from_static("application/json; charset=utf-8"),
   );
   headers.insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("private, no-store"),
   );
   headers.insert(
      "cloudflare-cdn-cache-control",
      HeaderValue::from_static("no-store"),
   );
   headers.insert("cdn-cache-control", HeaderValue::from_static("no-store"));
   response
}

/// One result: `{ "data": … }`.
#[derive(Serialize)]
struct One<T> {
   data: T,
}

/// A page of results: `{ "data": [ … ], "meta": { … } }`.
#[derive(Serialize)]
struct Page<T> {
   data: Vec<T>,
   meta: Meta,
}

#[derive(Serialize)]
struct Meta {
   result_count: usize,
   #[serde(skip_serializing_if = "Option::is_none")]
   cursor:       Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   query:        Option<String>,
}

impl<T> Page<T> {
   /// `next` is the cursor X returned and `sent` the one this request came
   /// with. A cursor that is empty, or that is the one just used, leads
   /// nowhere new, so the page is reported as the last.
   fn new(data: Vec<T>, next: Option<String>, sent: Option<&str>) -> Self {
      let cursor = next.filter(|next| !next.is_empty() && Some(next.as_str()) != sent);
      Self {
         meta: Meta {
            result_count: data.len(),
            cursor,
            query: None,
         },
         data,
      }
   }

   /// Report the search text that was sent to X, filters applied.
   fn with_query(mut self, query: String) -> Self {
      self.meta.query = Some(query);
      self
   }
}

fn flatten_posts(timeline: Timeline) -> Vec<Tweet> {
   let mut seen = std::collections::HashSet::new();
   let mut posts = Vec::new();
   for tweet in timeline.content.into_iter().flatten() {
      if tweet.id != 0 && !seen.insert(tweet.id) {
         continue;
      }
      posts.push(tweet);
   }
   posts
}

/// The names in a `tweet.fields` or `user.fields` parameter. `None` when it
/// was left out, which selects the endpoint's default.
fn field_names(raw: Option<&str>) -> Option<Vec<&str>> {
   let raw = raw.map(str::trim).filter(|value| !value.is_empty())?;
   Some(
      raw.split(',')
         .map(str::trim)
         .filter(|name| !name.is_empty())
         .collect(),
   )
}

#[derive(Clone, Copy)]
#[expect(clippy::struct_excessive_bools, reason = "one flag per response field")]
struct TweetFields {
   created_at: bool,
   lang:       bool,
   author:     bool,
   metrics:    bool,
   reply:      bool,
   media:      bool,
   quote:      bool,
   repost:     bool,
   sensitive:  bool,
   entities:   bool,
   card:       bool,
   poll:       bool,
   source:     bool,
   location:   bool,
   note:       bool,
   history:    bool,
   variants:   bool,
   labels:     bool,
}

impl TweetFields {
   const fn compact() -> Self {
      Self {
         created_at: true,
         lang:       true,
         author:     true,
         metrics:    true,
         reply:      true,
         media:      true,
         quote:      true,
         repost:     true,
         sensitive:  true,
         entities:   false,
         card:       false,
         poll:       false,
         source:     false,
         location:   false,
         note:       false,
         history:    false,
         variants:   false,
         labels:     false,
      }
   }

   const fn all() -> Self {
      Self {
         entities: true,
         card: true,
         poll: true,
         source: true,
         location: true,
         note: true,
         history: true,
         variants: true,
         labels: true,
         ..Self::compact()
      }
   }

   const fn none() -> Self {
      Self {
         created_at: false,
         lang:       false,
         author:     false,
         metrics:    false,
         reply:      false,
         media:      false,
         quote:      false,
         repost:     false,
         sensitive:  false,
         entities:   false,
         card:       false,
         poll:       false,
         source:     false,
         location:   false,
         note:       false,
         history:    false,
         variants:   false,
         labels:     false,
      }
   }

   fn parse(raw: Option<&str>) -> Checked<Self> {
      let Some(names) = field_names(raw) else {
         return Ok(Self::compact());
      };
      if names.contains(&"all") {
         if names.iter().any(|name| *name != "all") {
            return Err(Refusal::invalid(
               "tweet.fields all cannot be combined with other names.",
            ));
         }
         return Ok(Self::all());
      }
      let mut fields = Self::none();
      for name in names {
         if !fields.enable(name) && !matches!(name, "id" | "text") {
            return Err(Refusal::invalid(format!("Unknown tweet field \"{name}\".")));
         }
      }
      if fields.variants {
         fields.media = true;
      }
      Ok(fields)
   }

   fn enable(&mut self, token: &str) -> bool {
      let flag = match token {
         "created_at" => &mut self.created_at,
         "lang" => &mut self.lang,
         "author" => &mut self.author,
         "metrics" => &mut self.metrics,
         "reply" => &mut self.reply,
         "media" => &mut self.media,
         "quote" => &mut self.quote,
         "repost" => &mut self.repost,
         "sensitive" => &mut self.sensitive,
         "entities" => &mut self.entities,
         "card" => &mut self.card,
         "poll" => &mut self.poll,
         "source" => &mut self.source,
         "location" => &mut self.location,
         "note" => &mut self.note,
         "history" => &mut self.history,
         "variants" => &mut self.variants,
         "labels" => &mut self.labels,
         _ => return false,
      };
      *flag = true;
      true
   }

   fn nested(self) -> Self {
      Self {
         quote: false,
         entities: false,
         card: false,
         poll: false,
         source: false,
         location: false,
         note: false,
         history: false,
         variants: false,
         labels: false,
         ..self
      }
   }
}

#[derive(Clone, Copy)]
#[expect(clippy::struct_excessive_bools, reason = "one flag per profile field")]
struct UserFields {
   verified:  bool,
   avatar:    bool,
   banner:    bool,
   bio:       bool,
   location:  bool,
   website:   bool,
   followers: bool,
   following: bool,
   posts:     bool,
   likes:     bool,
   media:     bool,
   joined:    bool,
   pinned:    bool,
   protected: bool,
   suspended: bool,
}

impl UserFields {
   const fn stub() -> Self {
      Self {
         verified:  false,
         avatar:    false,
         banner:    false,
         bio:       false,
         location:  false,
         website:   false,
         followers: false,
         following: false,
         posts:     false,
         likes:     false,
         media:     false,
         joined:    false,
         pinned:    false,
         protected: false,
         suspended: false,
      }
   }

   const fn directory() -> Self {
      Self {
         verified: true,
         avatar: true,
         bio: true,
         location: true,
         website: true,
         followers: true,
         following: true,
         posts: true,
         protected: true,
         suspended: true,
         ..Self::stub()
      }
   }

   const fn full() -> Self {
      Self {
         verified:  true,
         avatar:    true,
         banner:    true,
         bio:       true,
         location:  true,
         website:   true,
         followers: true,
         following: true,
         posts:     true,
         likes:     true,
         media:     true,
         joined:    true,
         pinned:    true,
         protected: true,
         suspended: true,
      }
   }

   /// `default` is what the endpoint returns when `user.fields` is left out.
   /// Naming fields replaces it.
   fn parse(raw: Option<&str>, default: Self) -> Checked<Self> {
      let Some(names) = field_names(raw) else {
         return Ok(default);
      };
      if names.contains(&"all") {
         if names.iter().any(|name| *name != "all") {
            return Err(Refusal::invalid(
               "user.fields all cannot be combined with other names.",
            ));
         }
         return Ok(Self::full());
      }
      let mut fields = Self::stub();
      for name in names {
         if !fields.enable(name) && !matches!(name, "id" | "username" | "name") {
            return Err(Refusal::invalid(format!("Unknown user field \"{name}\".")));
         }
      }
      Ok(fields)
   }

   fn enable(&mut self, token: &str) -> bool {
      let flag = match token {
         "verified" => &mut self.verified,
         "avatar" => &mut self.avatar,
         "banner" => &mut self.banner,
         "bio" => &mut self.bio,
         "location" => &mut self.location,
         "website" => &mut self.website,
         "followers" => &mut self.followers,
         "following" => &mut self.following,
         "posts" => &mut self.posts,
         "likes" => &mut self.likes,
         "media" => &mut self.media,
         "joined" => &mut self.joined,
         "pinned" => &mut self.pinned,
         "protected" => &mut self.protected,
         "suspended" => &mut self.suspended,
         _ => return false,
      };
      *flag = true;
      true
   }
}

#[derive(Serialize)]
struct Account {
   id:              String,
   username:        String,
   #[serde(skip_serializing_if = "String::is_empty")]
   name:            String,
   #[serde(skip_serializing_if = "Option::is_none")]
   verified:        Option<&'static str>,
   #[serde(skip_serializing_if = "Option::is_none")]
   avatar_url:      Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   banner_url:      Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   bio:             Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   location:        Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   website:         Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   followers:       Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   following:       Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   posts:           Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   likes:           Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   media:           Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   joined_at:       Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   pinned_tweet_id: Option<String>,
   #[serde(skip_serializing_if = "is_false")]
   protected:       bool,
   #[serde(skip_serializing_if = "is_false")]
   suspended:       bool,
}

fn account(user: &User, fields: &UserFields) -> Account {
   let verified = fields
      .verified
      .then(|| verified_label(user.verified_type))
      .flatten();
   Account {
      id: user.id.clone(),
      username: user.username.clone(),
      name: user.fullname.clone(),
      verified,
      avatar_url: fields.avatar.then(|| link(&user.user_pic)).flatten(),
      // An account with no banner image carries its profile colour there,
      // as `#1DA1F2`. That is not a URL.
      banner_url: fields
         .banner
         .then(|| link(&user.banner))
         .flatten()
         .filter(|url| !url.starts_with('#')),
      // As X sends it, like a post's text. It is not HTML, so nothing is
      // stripped from it: a bio may say `<3`.
      bio: fields.bio.then(|| nonempty(user.bio.trim())).flatten(),
      location: fields.location.then(|| nonempty(&user.location)).flatten(),
      website: fields.website.then(|| nonempty(&user.website)).flatten(),
      followers: fields.followers.then_some(user.followers),
      following: fields.following.then_some(user.following),
      posts: fields.posts.then_some(user.tweets),
      likes: fields.likes.then_some(user.likes),
      media: fields.media.then_some(user.media),
      joined_at: fields.joined.then(|| stamp(user.join_date)).flatten(),
      pinned_tweet_id: fields
         .pinned
         .then(|| snowflake(user.pinned_tweet))
         .flatten(),
      protected: fields.protected && user.protected,
      suspended: fields.suspended && user.suspended,
   }
}

#[derive(Serialize)]
struct Post {
   id:                    String,
   text:                  String,
   #[serde(skip_serializing_if = "Option::is_none")]
   created_at:            Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   lang:                  Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   author:                Option<Account>,
   #[serde(skip_serializing_if = "Option::is_none")]
   metrics:               Option<Metrics>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   reply_to:              Vec<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   in_reply_to_status_id: Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   conversation_id:       Option<String>,
   #[serde(skip_serializing_if = "is_false")]
   sensitive:             bool,
   #[serde(skip_serializing_if = "Option::is_none")]
   unavailable:           Option<String>,
   #[serde(skip_serializing_if = "is_false")]
   pinned:                bool,
   #[serde(skip_serializing_if = "Option::is_none")]
   reposted_by:           Option<Account>,
   #[serde(skip_serializing_if = "Option::is_none")]
   repost_id:             Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   quote:                 Option<Box<Post>>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   media:                 Vec<Media>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   entities:              Vec<Ent>,
   #[serde(skip_serializing_if = "Option::is_none")]
   card:                  Option<LinkCard>,
   #[serde(skip_serializing_if = "Option::is_none")]
   poll:                  Option<PollOut>,
   #[serde(skip_serializing_if = "Option::is_none")]
   source:                Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   location:              Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   note:                  Option<String>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   history:               Vec<String>,
   #[serde(skip_serializing_if = "is_false")]
   paid_promotion:        bool,
   #[serde(skip_serializing_if = "is_false")]
   ai_generated:          bool,
}

fn post(tweet: &Tweet, fields: &TweetFields, users: &UserFields, nested: bool) -> Post {
   let fields = if nested { fields.nested() } else { *fields };
   let (shown, wrapper) = tweet
      .retweet
      .as_deref()
      .filter(|inner| inner.id != 0 || !inner.text.is_empty())
      .map_or((tweet, None), |inner| (inner, Some(tweet)));
   let repost = fields.repost.then_some(wrapper).flatten();
   Post {
      id:                    snowflake(shown.id).unwrap_or_else(|| "0".to_owned()),
      text:                  shown.text.clone(),
      created_at:            fields.created_at.then(|| stamp(shown.time)).flatten(),
      lang:                  fields.lang.then(|| nonempty(&shown.lang)).flatten(),
      author:                fields.author.then(|| account(&shown.user, users)),
      metrics:               fields.metrics.then(|| metrics(&shown.stats)).flatten(),
      reply_to:              fields
         .reply
         .then(|| shown.reply.clone())
         .unwrap_or_default(),
      in_reply_to_status_id: fields.reply.then(|| snowflake(shown.reply_id)).flatten(),
      conversation_id:       fields
         .reply
         .then(|| {
            (shown.thread_id != 0 && shown.thread_id != shown.id)
               .then(|| snowflake(shown.thread_id))
         })
         .flatten()
         .flatten(),
      sensitive:             fields.sensitive && shown.sensitive,
      unavailable:           (!shown.available).then(|| {
         if shown.tombstone.is_empty() {
            "unavailable".to_owned()
         } else {
            shown.tombstone.clone()
         }
      }),
      pinned:                shown.pinned,
      reposted_by:           repost
         .filter(|wrapper| !wrapper.user.username.is_empty())
         .map(|wrapper| account(&wrapper.user, users)),
      repost_id:             repost
         .filter(|wrapper| wrapper.id != 0 && wrapper.id != shown.id)
         .and_then(|wrapper| snowflake(wrapper.id)),
      quote:                 fields
         .quote
         .then(|| {
            shown
               .quote
               .as_deref()
               .filter(|quote| quote.id != 0 || !quote.text.is_empty())
               .map(|quote| Box::new(post(quote, &fields, users, true)))
         })
         .flatten(),
      media:                 fields
         .media
         .then(|| media_of(shown, fields.variants))
         .unwrap_or_default(),
      entities:              fields
         .entities
         .then(|| shown.entities.iter().map(ent).collect())
         .unwrap_or_default(),
      card:                  fields
         .card
         .then(|| shown.card.as_ref().and_then(card_of))
         .flatten(),
      poll:                  fields
         .poll
         .then(|| shown.poll.as_ref().and_then(poll_of))
         .flatten(),
      source:                fields.source.then(|| nonempty(&shown.source)).flatten(),
      location:              fields.location.then(|| nonempty(&shown.location)).flatten(),
      note:                  fields
         .note
         .then(|| shown.note.as_ref().and_then(|note| nonempty(&note.text)))
         .flatten(),
      history:               fields
         .history
         .then(|| history_of(shown))
         .unwrap_or_default(),
      paid_promotion:        fields.labels && shown.paid_promotion,
      ai_generated:          fields.labels && shown.ai_generated,
   }
}

#[derive(Serialize)]
struct Metrics {
   #[serde(skip_serializing_if = "zero_i64")]
   replies: i64,
   #[serde(skip_serializing_if = "zero_i64")]
   reposts: i64,
   #[serde(skip_serializing_if = "zero_i64")]
   likes:   i64,
   #[serde(skip_serializing_if = "zero_i64")]
   quotes:  i64,
   #[serde(skip_serializing_if = "zero_i64")]
   views:   i64,
}

fn metrics(stats: &crate::types::TweetStats) -> Option<Metrics> {
   let metrics = Metrics {
      replies: stats.replies,
      reposts: stats.retweets,
      likes:   stats.likes,
      quotes:  stats.quotes,
      views:   stats.views,
   };
   (metrics.replies != 0
      || metrics.reposts != 0
      || metrics.likes != 0
      || metrics.quotes != 0
      || metrics.views != 0)
      .then_some(metrics)
}

#[derive(Serialize)]
struct Media {
   #[serde(rename = "type")]
   kind:        &'static str,
   url:         String,
   #[serde(skip_serializing_if = "Option::is_none")]
   preview_url: Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   width:       Option<i32>,
   #[serde(skip_serializing_if = "Option::is_none")]
   height:      Option<i32>,
   #[serde(skip_serializing_if = "String::is_empty")]
   alt:         String,
   #[serde(skip_serializing_if = "Option::is_none")]
   duration_ms: Option<i32>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   variants:    Vec<Stream>,
}

#[derive(Serialize)]
struct Stream {
   content_type: &'static str,
   url:          String,
   #[serde(skip_serializing_if = "zero_i32")]
   bitrate:      i32,
   #[serde(skip_serializing_if = "zero_i32")]
   height:       i32,
}

fn media_of(tweet: &Tweet, variants: bool) -> Vec<Media> {
   let mut media = Vec::new();
   for photo in &tweet.photos {
      let Some(url) = link(&photo.url) else {
         continue;
      };
      media.push(Media {
         kind: "photo",
         url,
         preview_url: None,
         width: positive(photo.width),
         height: positive(photo.height),
         alt: photo.alt_text.clone(),
         duration_ms: None,
         variants: Vec::new(),
      });
   }
   for gif in &tweet.gifs {
      let Some(url) = link(&gif.url) else {
         continue;
      };
      media.push(Media {
         kind: "gif",
         url,
         preview_url: link(&gif.thumb),
         width: positive(gif.width),
         height: positive(gif.height),
         alt: gif.alt_text.clone(),
         duration_ms: None,
         variants: Vec::new(),
      });
   }
   if let Some(video) = tweet.video.as_ref() {
      media.extend(video_media(video, variants));
   }
   for video in &tweet.additional_videos {
      media.extend(video_media(video, variants));
   }
   media
}

fn video_media(video: &Video, variants: bool) -> Option<Media> {
   let url = video
      .best_mp4_url()
      .and_then(link)
      .or_else(|| link(&video.url))?;
   let (width, height) = video_size(video);
   Some(Media {
      kind: "video",
      url,
      preview_url: link(&video.thumb),
      width,
      height,
      alt: String::new(),
      duration_ms: positive(video.duration_ms),
      variants: variants
         .then(|| {
            video
               .variants
               .iter()
               .filter_map(|variant| {
                  let url = link(&variant.url)?;
                  Some(Stream {
                     content_type: match variant.content_type {
                        VideoType::Mp4 => "video/mp4",
                        VideoType::M3u8 => "application/x-mpegURL",
                     },
                     url,
                     bitrate: variant.bitrate,
                     height: variant.resolution,
                  })
               })
               .collect()
         })
         .unwrap_or_default(),
   })
}

/// The size the video's own URL states, else the height of its best MP4.
/// Never a guess: a missing dimension is left out.
fn video_size(video: &Video) -> (Option<i32>, Option<i32>) {
   let url = video.best_mp4_url().unwrap_or(video.url.as_str());
   if let Some((width, height)) = Video::dimensions_from_url(url) {
      return (Some(width), Some(height));
   }
   video
      .variants
      .iter()
      .filter(|variant| matches!(variant.content_type, VideoType::Mp4))
      .max_by_key(|variant| variant.resolution)
      .and_then(|variant| positive(variant.resolution))
      .map_or((None, None), |height| (None, Some(height)))
}

#[derive(Serialize)]
struct Ent {
   #[serde(rename = "type")]
   kind:  &'static str,
   start: usize,
   end:   usize,
   #[serde(skip_serializing_if = "String::is_empty")]
   text:  String,
   #[serde(skip_serializing_if = "String::is_empty")]
   url:   String,
}

fn ent(entity: &crate::types::Entity) -> Ent {
   Ent {
      kind:  match entity.kind {
         EntityKind::Url => "url",
         EntityKind::Mention => "mention",
         EntityKind::Hashtag => "hashtag",
         EntityKind::Symbol => "symbol",
      },
      start: entity.indices.0,
      end:   entity.indices.1,
      text:  entity.display.clone(),
      url:   entity.url.clone(),
   }
}

#[derive(Serialize)]
struct LinkCard {
   #[serde(rename = "type")]
   kind:        &'static str,
   #[serde(skip_serializing_if = "String::is_empty")]
   url:         String,
   #[serde(skip_serializing_if = "String::is_empty")]
   title:       String,
   #[serde(skip_serializing_if = "String::is_empty")]
   description: String,
   #[serde(skip_serializing_if = "Option::is_none")]
   image:       Option<String>,
}

fn card_of(card: &Card) -> Option<LinkCard> {
   let url = if card.url.is_empty() {
      card.dest.clone()
   } else {
      card.url.clone()
   };
   let image = link(&card.image);
   if url.is_empty() && card.title.is_empty() && card.text.is_empty() && image.is_none() {
      return None;
   }
   Some(LinkCard {
      kind: card_kind(card.kind),
      url,
      title: card.title.clone(),
      description: card.text.clone(),
      image,
   })
}

fn card_kind(kind: CardKind) -> &'static str {
   match kind {
      CardKind::Summary => "summary",
      CardKind::SummaryLarge => "summary_large_image",
      CardKind::Player => "player",
      CardKind::Article => "article",
      CardKind::Amplify => "amplify",
      CardKind::Broadcast | CardKind::Periscope | CardKind::Audiospace => "broadcast",
      _ => "link",
   }
}

#[derive(Serialize)]
struct PollOut {
   options: Vec<Choice>,
   votes:   i64,
   #[serde(skip_serializing_if = "Option::is_none")]
   ends_at: Option<String>,
}

#[derive(Serialize)]
struct Choice {
   label: String,
   votes: i64,
}

fn poll_of(poll: &Poll) -> Option<PollOut> {
   if poll.options.is_empty() {
      return None;
   }
   Some(PollOut {
      options: poll
         .options
         .iter()
         .enumerate()
         .map(|(index, label)| {
            Choice {
               label: label.clone(),
               votes: poll.values.get(index).copied().unwrap_or(0),
            }
         })
         .collect(),
      votes:   poll.votes,
      ends_at: stamp(poll.end_time),
   })
}

#[derive(Serialize)]
struct ListRow<'a> {
   id:          &'a str,
   name:        &'a str,
   #[serde(skip_serializing_if = "empty_ref")]
   description: &'a str,
   #[serde(skip_serializing_if = "empty_ref")]
   username:    &'a str,
   #[serde(skip_serializing_if = "zero_i32")]
   members:     i32,
   #[serde(skip_serializing_if = "zero_i32")]
   subscribers: i32,
}

fn list_row(list: &List) -> ListRow<'_> {
   ListRow {
      id:          &list.id,
      name:        &list.name,
      description: &list.description,
      username:    &list.username,
      members:     list.members,
      subscribers: list.subscribers,
   }
}

fn history_of(tweet: &Tweet) -> Vec<String> {
   let ids = tweet
      .history
      .iter()
      .copied()
      .filter(|id| *id != 0)
      .map(|id| id.to_string())
      .collect::<Vec<_>>();
   if ids.len() <= 1 { Vec::new() } else { ids }
}

fn snowflake(id: i64) -> Option<String> {
   (id != 0).then(|| id.to_string())
}

fn stamp(time: Option<time::OffsetDateTime>) -> Option<String> {
   time.and_then(|time| {
      time
         .format(&time::format_description::well_known::Rfc3339)
         .ok()
   })
}

fn link(url: &str) -> Option<String> {
   let url = direct_media_url(url);
   (!url.is_empty()).then_some(url)
}

fn nonempty(value: &str) -> Option<String> {
   (!value.is_empty()).then(|| value.to_owned())
}

fn positive(value: i32) -> Option<i32> {
   (value > 0).then_some(value)
}

fn verified_label(kind: VerifiedType) -> Option<&'static str> {
   match kind {
      VerifiedType::None => None,
      VerifiedType::Blue => Some("blue"),
      VerifiedType::Business => Some("business"),
      VerifiedType::Government => Some("government"),
   }
}

fn empty_ref(value: &&str) -> bool {
   value.is_empty()
}

fn is_false(value: &bool) -> bool {
   !*value
}

fn zero_i64(value: &i64) -> bool {
   *value == 0
}

fn zero_i32(value: &i32) -> bool {
   *value == 0
}

#[cfg(test)]
mod tests {
   use axum::{
      body::Body,
      http::Request,
   };
   use http_body_util::BodyExt as _;
   use tower::ServiceExt as _;

   use super::*;
   use crate::types::{
      Photo,
      TweetStats,
      VideoVariant,
   };

   fn user(username: &str) -> User {
      User {
         id: "7".into(),
         username: username.into(),
         fullname: "Full".into(),
         ..User::default()
      }
   }

   #[test]
   fn ids_stay_strings_and_empty_fields_are_left_out() {
      let tweet = Tweet {
         id: 9_007_199_254_740_993,
         text: "hello".into(),
         user: user("ada"),
         stats: TweetStats {
            likes: 3,
            ..TweetStats::default()
         },
         photos: vec![Photo {
            url: "/media/cat.jpg".into(),
            alt_text: "cat".into(),
            width: 10,
            height: 20,
            ..Photo::default()
         }],
         ..Tweet::default()
      };
      let value = serde_json::to_value(post(
         &tweet,
         &TweetFields::compact(),
         &UserFields::stub(),
         false,
      ))
      .unwrap();
      assert_eq!(value["id"], "9007199254740993");
      assert_eq!(value["author"]["username"], "ada");
      assert!(value.get("bio").is_none());
      assert!(value["author"].get("avatar_url").is_none());
      assert_eq!(value["metrics"]["likes"], 3);
      assert!(value["metrics"].get("replies").is_none());
      assert_eq!(value["media"][0]["type"], "photo");
      assert_eq!(
         value["media"][0]["url"],
         "https://pbs.twimg.com/media/cat.jpg"
      );
      assert!(value.get("card").is_none());
      assert!(value.get("quote").is_none());
   }

   #[test]
   fn a_repost_is_the_original_plus_who_reposted_it() {
      let original = Tweet {
         id: 9,
         text: "hello".into(),
         user: user("bob"),
         ..Tweet::default()
      };
      let wrapper = Tweet {
         id: 8,
         user: user("ann"),
         retweet: Some(Box::new(original)),
         ..Tweet::default()
      };
      let value = serde_json::to_value(post(
         &wrapper,
         &TweetFields::compact(),
         &UserFields::stub(),
         false,
      ))
      .unwrap();
      assert_eq!(value["id"], "9");
      assert_eq!(value["text"], "hello");
      assert_eq!(value["reposted_by"]["username"], "ann");
      assert_eq!(value["repost_id"], "8");
   }

   #[test]
   fn a_quote_does_not_include_its_own_quote() {
      let inner = Tweet {
         id: 3,
         text: "inner".into(),
         ..Tweet::default()
      };
      let mid = Tweet {
         id: 2,
         text: "mid".into(),
         quote: Some(Box::new(inner)),
         ..Tweet::default()
      };
      let outer = Tweet {
         id: 1,
         text: "outer".into(),
         quote: Some(Box::new(mid)),
         ..Tweet::default()
      };
      let value = serde_json::to_value(post(
         &outer,
         &TweetFields::compact(),
         &UserFields::stub(),
         false,
      ))
      .unwrap();
      assert_eq!(value["quote"]["text"], "mid");
      assert!(value["quote"].get("quote").is_none());
   }

   #[test]
   fn naming_fields_replaces_the_default() {
      assert!(TweetFields::parse(None).unwrap().created_at);
      assert!(!TweetFields::parse(Some("source")).unwrap().created_at);
      assert!(TweetFields::parse(Some("source")).unwrap().source);
      assert!(TweetFields::parse(Some("variants")).unwrap().media);
      assert!(TweetFields::parse(Some("all")).unwrap().card);
      assert!(TweetFields::parse(Some("nope")).is_err());
      assert!(
         UserFields::parse(Some("avatar"), UserFields::stub())
            .unwrap()
            .avatar
      );
      assert!(UserFields::parse(Some("nope"), UserFields::stub()).is_err());
   }

   #[test]
   fn video_variants_are_opt_in() {
      let tweet = Tweet {
         id: 1,
         text: "clip".into(),
         video: Some(Video {
            url: "https://video.twimg.com/video.mp4".into(),
            variants: vec![VideoVariant {
               content_type: VideoType::Mp4,
               url:          "https://video.twimg.com/vid/1280x720/clip.mp4".into(),
               bitrate:      1,
               resolution:   720,
            }],
            ..Video::default()
         }),
         ..Tweet::default()
      };
      let compact = serde_json::to_value(post(
         &tweet,
         &TweetFields::compact(),
         &UserFields::stub(),
         false,
      ))
      .unwrap();
      assert!(compact["media"][0].get("variants").is_none());
      assert_eq!(compact["media"][0]["width"], 1280);
      let full = serde_json::to_value(post(
         &tweet,
         &TweetFields::parse(Some("media,variants")).unwrap(),
         &UserFields::stub(),
         false,
      ))
      .unwrap();
      assert_eq!(full["media"][0]["variants"][0]["height"], 720);
   }

   #[test]
   fn unknown_filter_names_are_reported() {
      let params = SearchQuery {
         filters: Some("images,not-real".into()),
         ..SearchQuery::default()
      };
      assert_eq!(unknown_named_filter(&params).as_deref(), Some("not-real"));
      let ok = SearchQuery {
         filters: Some("images,links".into()),
         exclude: Some("replies".into()),
         ..SearchQuery::default()
      };
      assert!(unknown_named_filter(&ok).is_none());
   }

   #[test]
   fn search_dates_and_counts_are_checked_before_they_reach_x() {
      let usable = SearchQuery {
         since: Some("2024-01-05".into()),
         until: Some(String::new()),
         min_faves: Some("10".into()),
         ..SearchQuery::default()
      };
      assert!(check_query(&usable).is_ok());
      for unusable in [
         SearchQuery {
            since: Some("2024-1-5".into()),
            ..SearchQuery::default()
         },
         SearchQuery {
            until: Some("2024-01-05 OR from:x".into()),
            ..SearchQuery::default()
         },
         SearchQuery {
            min_faves: Some("ten".into()),
            ..SearchQuery::default()
         },
      ] {
         assert!(check_query(&unusable).is_err());
      }
   }

   #[test]
   fn a_batch_names_each_post_once_the_way_x_writes_it() {
      assert_eq!(batch_ids(Some("20, 21,20\n0022")).unwrap(), [
         "20", "21", "22"
      ]);
      for unusable in [None, Some(" "), Some("20,abc"), Some("0")] {
         assert!(batch_ids(unusable).is_err(), "{unusable:?}");
      }
      let too_many = (1..=MAX_BATCH + 1)
         .map(|id| id.to_string())
         .collect::<Vec<_>>()
         .join(",");
      assert!(batch_ids(Some(&too_many)).is_err());
   }

   #[test]
   fn a_post_names_its_author_and_a_list_of_accounts_describes_them() {
      let params = PageParams::default();
      let (_, author) = params.post_fields().unwrap();
      assert!(!author.avatar && !author.bio && !author.followers);
      let listed = params.account_fields().unwrap();
      assert!(listed.avatar && listed.bio && listed.followers);

      let asked = PageParams {
         user_fields: Some("verified".into()),
         ..PageParams::default()
      };
      assert!(asked.post_fields().unwrap().1.verified);
      assert!(!asked.account_fields().unwrap().bio);
   }

   #[test]
   fn a_banner_colour_is_not_a_url_and_a_bio_is_left_as_written() {
      let mut ada = user("ada");
      ada.banner = "#1DA1F2".into();
      ada.bio = " I <3 rust > sleep ".into();
      let value = serde_json::to_value(account(&ada, &UserFields::full())).unwrap();
      assert!(value.get("banner_url").is_none());
      assert_eq!(value["bio"], "I <3 rust > sleep");

      ada.banner = "https://pbs.twimg.com/profile_banners/7/1/1500x500".into();
      let value = serde_json::to_value(account(&ada, &UserFields::full())).unwrap();
      assert_eq!(
         value["banner_url"],
         "https://pbs.twimg.com/profile_banners/7/1/1500x500"
      );
   }

   #[test]
   fn a_cursor_that_leads_nowhere_ends_the_list() {
      let page = Page::new(vec![1], Some("next".into()), Some("sent"));
      assert_eq!(page.meta.cursor.as_deref(), Some("next"));
      assert_eq!(page.meta.result_count, 1);
      // The cursor this request came with, an empty one, and none at all.
      for (next, sent) in [
         (Some("sent".to_owned()), Some("sent")),
         (Some(String::new()), None),
         (None, None),
      ] {
         assert!(Page::new(vec![1], next, sent).meta.cursor.is_none());
      }

      assert_eq!(
         accounts_cursor(Some("123|456".into()), 20).as_deref(),
         Some("123|456")
      );
      assert!(accounts_cursor(Some("0|456".into()), 20).is_none());
      assert!(accounts_cursor(Some("123|456".into()), 0).is_none());
   }

   #[test]
   fn one_result_and_a_page_write_a_post_the_same_way() {
      let tweet = Tweet {
         id: 1,
         text: "hello".into(),
         available: true,
         ..Tweet::default()
      };
      let fields = TweetFields::parse(Some("id")).unwrap();
      let post = || post(&tweet, &fields, &UserFields::stub(), false);
      assert_eq!(
         serde_json::to_string(&One { data: post() }).unwrap(),
         r#"{"data":{"id":"1","text":"hello"}}"#
      );
      assert_eq!(
         serde_json::to_string(&Page::new(vec![post()], None, None)).unwrap(),
         r#"{"data":[{"id":"1","text":"hello"}],"meta":{"result_count":1}}"#
      );
   }

   #[test]
   fn a_list_row_leaves_out_what_is_empty() {
      let row = ListRow {
         id:          "1",
         name:        "name",
         description: "",
         username:    "",
         members:     0,
         subscribers: 2,
      };
      let value = serde_json::to_value(row).unwrap();
      assert!(value.get("description").is_none());
      assert_eq!(value["subscribers"], 2);
   }

   #[test]
   fn only_the_routes_of_this_api_are_owned() {
      for path in [
         "/api",
         "/api/openapi.json",
         "/api/search",
         "/api/lists/1/tweets",
         "/api/communities/1/tweets",
         "/api/users/id/12",
         "/api/users/jack",
         "/api/users/jack/tweets",
         "/api/users/jack/with_replies",
         "/api/users/jack/about",
         "/api/users/jack/following",
         "/api/users/jack/following/ids",
         "/api/users/jack/followers",
         "/api/users/jack/friends",
         "/api/tweets",
         "/api/tweets/20",
         "/api/tweets/20/retweeters",
      ] {
         assert!(owns(path), "{path}");
      }
      for path in [
         "/",
         "/search",
         "/apiary",
         "/jack/api",
         "/api/",
         "/api/users",
         "/api/users/jack/media",
         "/api/tweets/20/quotes",
         // The embed endpoint, and pages of an account named `api`.
         "/api/v1/statuses/20",
         "/api/search/rss",
         "/api/lists/rust",
      ] {
         assert!(!owns(path), "{path}");
      }
   }

   #[test]
   fn an_error_keeps_its_documented_code_and_status() {
      for (err, status, code) in [
         (Error::UserNotFound("x".into()), 404, "not_found"),
         (Error::TweetNotFound("x".into()), 404, "not_found"),
         (Error::UserSuspended("x".into()), 403, "suspended"),
         (Error::ProtectedUser("x".into()), 403, "protected"),
         (Error::SearchDenylisted, 400, "blocked"),
         (Error::InvalidUrl("x".into()), 400, "invalid"),
         (Error::RateLimited, 429, "rate_limited"),
         (Error::ClientBudgetExhausted, 429, "rate_limited"),
         (Error::TransientUpstream, 503, "unavailable"),
         (Error::NoSessions, 503, "unavailable"),
         (Error::Internal("x".into()), 500, "error"),
      ] {
         let refusal = Refusal::from(err);
         assert_eq!((refusal.status.as_u16(), refusal.code), (status, code));
      }
   }

   #[test]
   fn both_descriptions_name_every_code_a_refusal_can_carry() {
      let page = views::json_api::render("https://xitter.cf", false).into_string();
      let spec = super::super::json_openapi::document("https://xitter.cf").to_string();
      for code in [
         "pass_required",
         "invalid",
         "blocked",
         "not_found",
         "suspended",
         "protected",
         "rate_limited",
         "unavailable",
         "error",
      ] {
         assert!(
            page.contains(&format!("<code>{code}</code>")),
            "/api does not list {code}"
         );
         assert!(
            spec.contains(&format!("{code} (")),
            "the OpenAPI description does not list {code}"
         );
      }
      // The origin is the one the reader came through, scheme included.
      let local = views::json_api::render("http://localhost:8080", true).into_string();
      assert!(local.contains("http://localhost:8080/api/search?q=rust"));
   }

   async fn body_of(response: Response) -> serde_json::Value {
      let bytes = response.into_body().collect().await.unwrap().to_bytes();
      serde_json::from_slice(&bytes).unwrap()
   }

   #[tokio::test]
   async fn every_refusal_is_json_in_one_shape() {
      let no_pass = Refusal::pass_required(false).into_response();
      assert_eq!(no_pass.status(), StatusCode::UNAUTHORIZED);
      assert_eq!(no_pass.headers()[header::WWW_AUTHENTICATE], "Bearer");
      assert_eq!(
         no_pass.headers()[header::CONTENT_TYPE],
         "application/json; charset=utf-8"
      );
      assert_eq!(
         no_pass.headers()[header::CACHE_CONTROL],
         "private, no-store"
      );
      assert_eq!(no_pass.headers()["cdn-cache-control"], "no-store");
      let body = body_of(no_pass).await;
      assert_eq!(body["errors"].as_array().unwrap().len(), 1);
      assert_eq!(body["errors"][0]["code"], "pass_required");
      let message = body["errors"][0]["message"].as_str().unwrap();
      assert!(
         message.starts_with("A search pass is required."),
         "{message}"
      );

      // A key that was sent is named, so nobody hunts for a missing header.
      let refused_key = body_of(Refusal::pass_required(true).into_response()).await;
      let message = refused_key["errors"][0]["message"].as_str().unwrap();
      assert!(
         message.starts_with("That API key was not accepted."),
         "{message}"
      );

      let limited = Refusal::from(Error::RateLimited).into_response();
      assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
      assert_eq!(limited.headers()[header::RETRY_AFTER], "60");
      assert!(limited.headers().get(header::WWW_AUTHENTICATE).is_none());
      assert_eq!(body_of(limited).await["errors"][0]["code"], "rate_limited");

      let bad_id = invalid_id();
      assert_eq!(bad_id.status(), StatusCode::BAD_REQUEST);
      assert_eq!(body_of(bad_id).await["errors"][0]["code"], "invalid");
   }

   #[tokio::test]
   async fn a_query_string_that_cannot_be_read_is_refused_in_json() {
      let app = Router::new().route(
         "/",
         get(|ApiQuery(params): ApiQuery<PageParams>| {
            async move { params.count.unwrap_or(0).to_string() }
         }),
      );
      let ask = |uri: &'static str| {
         app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
      };

      let read = ask("/?count=7&tweet.fields=id").await.unwrap();
      assert_eq!(read.status(), StatusCode::OK);
      for uri in ["/?count=many", "/?cursor=a&cursor=b"] {
         let refused = ask(uri).await.unwrap();
         assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{uri}");
         assert_eq!(
            body_of(refused).await["errors"][0]["code"],
            "invalid",
            "{uri}"
         );
      }
   }
}
