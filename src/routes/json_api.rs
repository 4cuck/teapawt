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

use std::{
   any::Any,
   future::Future,
};

use axum::{
   Router,
   extract::{
      Path,
      Query,
      State,
   },
   http::{
      HeaderValue,
      StatusCode,
      header,
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
};
use serde_json::json;

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

async fn openapi_doc(State(state): State<AppState>) -> Response {
   let origin = crate::utils::host::current_origin(state.config.url_prefix());
   let body = super::json_openapi::document(&origin);
   (
      [
         (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
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
   let prefs = crate::types::Prefs::from_cookies(&jar, &state.config);
   let host = crate::utils::host::request_origin().map_or_else(
      || crate::utils::host::host_label(state.config.url_prefix()).to_owned(),
      |value| crate::utils::host::host_label(&value).to_owned(),
   );
   let content = views::json_api::render(&host, state.passes.admits(&jar));
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

async fn search(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(params): Query<SearchParams>,
) -> Result<Response> {
   if let Err(response) = admit(&state, &jar) {
      return Ok(response);
   }
   if let Some(name) = unknown_named_filter(&params.search) {
      return Ok(invalid(format!("Unknown filter \"{name}\".")));
   }
   let tweets = match TweetFields::parse(params.tweet_fields.as_deref()) {
      Ok(fields) => fields,
      Err(message) => return Ok(invalid(message)),
   };
   let tab = match tab(params.search.filter.as_deref()) {
      Ok(tab) => tab,
      Err(response) => return Ok(response),
   };
   let users = match UserFields::parse(
      params.user_fields.as_deref(),
      if tab == "users" {
         UserFields::directory()
      } else {
         UserFields::stub()
      },
   ) {
      Ok(fields) => fields,
      Err(message) => return Ok(invalid(message)),
   };
   if let Err(response) = check_query(&params.search) {
      return Ok(response);
   }
   let cursor = match cursor(params.search.cursor.as_deref()) {
      Ok(cursor) => cursor,
      Err(response) => return Ok(response),
   };

   if tab == "users" || tab == "lists" {
      let query = params.search.query.clone().unwrap_or_default();
      if query.trim().is_empty() {
         return Ok(invalid("q is required."));
      }
   }
   if tab == "users" {
      let query = params.search.query.clone().unwrap_or_default();
      return match load_users(&state, &query, cursor).await {
         Ok(result) => {
            let empty = result.content.is_empty();
            for user in &result.content {
               helpers::remember_user_id(&state, user);
            }
            let body = page(
               result
                  .content
                  .iter()
                  .map(|user| account(user, &users))
                  .collect(),
               result.bottom,
               None,
            );
            Ok(admin_stats::search_result(json_ok(&body), empty))
         },
         Err(err) => Ok(fail(&err)),
      };
   }
   if tab == "lists" {
      let query = params.search.query.clone().unwrap_or_default();
      return match load_lists(&state, &query, cursor).await {
         Ok(result) => {
            let empty = result.content.is_empty();
            let body = page(
               result.content.iter().map(list_row).collect(),
               result.bottom,
               None,
            );
            Ok(admin_stats::search_result(json_ok(&body), empty))
         },
         Err(err) => Ok(fail(&err)),
      };
   }

   let built = params.search.to_query().build();
   if built.trim().is_empty() {
      return Ok(invalid("q is required."));
   }
   if built.chars().count() > MAX_QUERY.saturating_mul(2) {
      return Ok(invalid("That search is too long."));
   }
   let product = match tab {
      "top" => "Top",
      "media" => "Media",
      _ => "Latest",
   };
   match load_timeline(&state, &built, product, cursor).await {
      Ok(timeline) => {
         let next = timeline.bottom.clone();
         let mut posts =
            super::search::dedup_search_tweets(timeline.content.into_iter().flatten().collect());
         if tab == "media" {
            posts.retain(Tweet::has_gallery_media);
         }
         let empty = posts.is_empty();
         helpers::remember_tweets(&state, &posts);
         let body = page(
            posts
               .iter()
               .map(|tweet| post(tweet, &tweets, &users, false))
               .collect(),
            next,
            Some(built),
         );
         Ok(admin_stats::search_result(json_ok(&body), empty))
      },
      Err(err) => Ok(fail(&err)),
   }
}

async fn user(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
) -> Result<Response> {
   if let Err(response) = admit(&state, &jar) {
      return Ok(response);
   }
   let Ok(name) = handle(&username) else {
      return Ok(invalid("username is not a valid X handle."));
   };
   match helpers::get_cached_user(&state, name).await {
      Ok(user) => Ok(json_ok(&json!({ "data": account(&user, &UserFields::full()) }))),
      Err(err) => Ok(fail(&err)),
   }
}

async fn user_tweets(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   if let Err(response) = admit(&state, &jar) {
      return Ok(response);
   }
   let Ok(name) = handle(&username) else {
      return Ok(invalid("username is not a valid X handle."));
   };
   let tweets = match TweetFields::parse(params.tweet_fields.as_deref()) {
      Ok(fields) => fields,
      Err(message) => return Ok(invalid(message)),
   };
   let users = match UserFields::parse(params.user_fields.as_deref(), UserFields::stub()) {
      Ok(fields) => fields,
      Err(message) => return Ok(invalid(message)),
   };
   let cursor = match cursor(params.cursor.as_deref()) {
      Ok(cursor) => cursor,
      Err(response) => return Ok(response),
   };
   let user = match helpers::get_cached_user(&state, name).await {
      Ok(user) => user,
      Err(err) => return Ok(fail(&err)),
   };
   if user.id.is_empty() {
      return Ok(fail(&Error::UserNotFound(name.to_owned())));
   }
   if user.suspended {
      return Ok(fail(&Error::UserSuspended(user.username)));
   }
   if user.protected {
      return Ok(fail(&Error::ProtectedUser(user.username)));
   }
   match load_user_posts(&state, name, &user.id, cursor).await {
      Ok(timeline) => {
         let next = timeline.bottom.clone();
         let posts = flatten_posts(timeline);
         helpers::remember_tweets(&state, &posts);
         Ok(json_ok(&page(
            posts
               .iter()
               .map(|tweet| post(tweet, &tweets, &users, false))
               .collect(),
            next,
            None,
         )))
      },
      Err(err) => Ok(fail(&err)),
   }
}

async fn tweet(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   if let Err(response) = admit(&state, &jar) {
      return Ok(response);
   }
   let tweets = match TweetFields::parse(params.tweet_fields.as_deref()) {
      Ok(fields) => fields,
      Err(message) => return Ok(invalid(message)),
   };
   let users = match UserFields::parse(params.user_fields.as_deref(), UserFields::stub()) {
      Ok(fields) => fields,
      Err(message) => return Ok(invalid(message)),
   };
   match helpers::get_cached_tweet(&state, &id).await {
      Ok(tweet) => {
         helpers::remember_tweets(&state, std::slice::from_ref(&tweet));
         Ok(json_ok(&json!({ "data": post(&tweet, &tweets, &users, false) })))
      },
      Err(err) => Ok(fail(&err)),
   }
}

async fn list_tweets(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   let Some(id) = snowflake_param(&id) else {
      return Ok(invalid("id is not a list id."));
   };
   let (tweets, users, cursor) = match page_fields(&state, &jar, &params) {
      Ok(ready) => ready,
      Err(response) => return Ok(response),
   };
   let key = cache_keys::list_tweets(id, cursor);
   let api = state.clone();
   let list_id = id.to_owned();
   let page_cursor = cursor.map(str::to_owned);
   match cached(&state, &key, async move {
      api.api
         .get_list_tweets(&list_id, page_cursor.as_deref())
         .await
   })
   .await {
      Ok(timeline) => Ok(posts_body(&state, timeline, &tweets, &users)),
      Err(err) => Ok(fail(&err)),
   }
}

async fn community_tweets(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   let Some(id) = snowflake_param(&id) else {
      return Ok(invalid("id is not a community id."));
   };
   let ranking = match params.ranking.as_deref().map(str::trim) {
      None | Some("" | "latest" | "recency") => "Recency",
      Some("top" | "relevance") => "Relevance",
      Some(_) => return Ok(invalid("ranking must be latest or top.")),
   };
   let (tweets, users, cursor) = match page_fields(&state, &jar, &params) {
      Ok(ready) => ready,
      Err(response) => return Ok(response),
   };
   let key = format!("ct:{id}:{ranking}:{}", cursor.unwrap_or(""));
   let api = state.clone();
   let community_id = id.to_owned();
   let page_cursor = cursor.map(str::to_owned);
   let mode = ranking.to_owned();
   match cached(&state, &key, async move {
      api.api
         .get_community_tweets(&community_id, page_cursor.as_deref(), &mode)
         .await
   })
   .await
   {
      Ok(timeline) => Ok(posts_body(&state, timeline, &tweets, &users)),
      Err(err) => Ok(fail(&err)),
   }
}

async fn user_by_id(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
) -> Result<Response> {
   if let Err(response) = admit(&state, &jar) {
      return Ok(response);
   }
   let Some(id) = snowflake_param(&id) else {
      return Ok(invalid("id is not a user id."));
   };
   match state.api.get_user_by_id(id).await {
      Ok(user) => {
         helpers::remember_user_id(&state, &user);
         Ok(json_ok(&json!({ "data": account(&user, &UserFields::full()) })))
      },
      Err(err) => Ok(fail(&err)),
   }
}

async fn user_replies(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   let (tweets, users, cursor) = match page_fields(&state, &jar, &params) {
      Ok(ready) => ready,
      Err(response) => return Ok(response),
   };
   let Ok(name) = handle(&username) else {
      return Ok(invalid("username is not a valid X handle."));
   };
   let user = match helpers::get_cached_user(&state, name).await {
      Ok(user) if !user.id.is_empty() => user,
      Ok(_) => return Ok(fail(&Error::UserNotFound(name.to_owned()))),
      Err(err) => return Ok(fail(&err)),
   };
   let key = format!("ur:{}:{}", user.id, cursor.unwrap_or(""));
   let api = state.clone();
   let user_id = user.id.clone();
   let page_cursor = cursor.map(str::to_owned);
   match cached(&state, &key, async move {
      api.api
         .get_user_tweets_and_replies(&user_id, page_cursor.as_deref())
         .await
   })
   .await
   {
      Ok(timeline) => Ok(posts_body(&state, timeline, &tweets, &users)),
      Err(err) => Ok(fail(&err)),
   }
}

async fn about(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
) -> Result<Response> {
   if let Err(response) = admit(&state, &jar) {
      return Ok(response);
   }
   let Ok(name) = handle(&username) else {
      return Ok(invalid("username is not a valid X handle."));
   };
   let key = cache_keys::account_context(&name.to_lowercase());
   let context = if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) =
      state.cache.lookup::<crate::types::AccountContext>(&key)
   {
      hit
   } else {
      match state.api.get_account_context(name).await {
         Ok(context) => {
            state.cache.set(&key, &context, ttl::ACCOUNT_CONTEXT);
            context
         },
         Err(err) => return Ok(fail(&err)),
      }
   };
   let mut data = serde_json::Map::new();
   if !context.account_based_in.is_empty() {
      data.insert(
         "account_based_in".into(),
         json!(context.account_based_in),
      );
   }
   if !context.connection_source.is_empty() {
      data.insert(
         "connection_source".into(),
         json!(context.connection_source),
      );
   }
   if let Some(accurate) = context.location_accurate {
      data.insert("location_accurate".into(), json!(accurate));
   }
   Ok(json_ok(&json!({ "data": data })))
}

async fn following(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   user_graph(&state, &jar, &username, &params, Graph::Following).await
}

async fn following_ids(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   user_graph(&state, &jar, &username, &params, Graph::Ids).await
}

async fn followers(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   user_graph(&state, &jar, &username, &params, Graph::Followers).await
}

async fn friends(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   user_graph(&state, &jar, &username, &params, Graph::Friends).await
}

async fn retweeters(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   let Some(id) = snowflake_param(&id) else {
      return Ok(invalid("id is not a post id."));
   };
   let (_tweets, users, cursor) = match page_fields(&state, &jar, &params) {
      Ok(ready) => ready,
      Err(response) => return Ok(response),
   };
   let key = format!("rt:{id}:{}", cursor.unwrap_or(""));
   let api = state.clone();
   let tweet_id = id.to_owned();
   let page_cursor = cursor.map(str::to_owned);
   match cached(&state, &key, async move {
      api.api
         .get_retweeters(&tweet_id, page_cursor.as_deref())
         .await
   })
   .await {
      Ok(result) => {
         for user in &result.content {
            helpers::remember_user_id(&state, user);
         }
         Ok(json_ok(&page(
            result.content.iter().map(|user| account(user, &users)).collect(),
            result.bottom,
            None,
         )))
      },
      Err(err) => Ok(fail(&err)),
   }
}

async fn tweets(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(params): Query<PageParams>,
) -> Result<Response> {
   let (tweets, users, _) = match page_fields(&state, &jar, &params) {
      Ok(ready) => ready,
      Err(response) => return Ok(response),
   };
   let Some(raw) = params.ids.as_deref().map(str::trim).filter(|ids| !ids.is_empty()) else {
      return Ok(invalid("ids is required."));
   };
   let mut ids = Vec::new();
   for id in raw.split(|ch: char| ch == ',' || ch.is_whitespace()) {
      let id = id.trim();
      if id.is_empty() {
         continue;
      }
      let Some(id) = snowflake_param(id) else {
         return Ok(invalid("ids must be post ids."));
      };
      ids.push(id.to_owned());
   }
   if ids.is_empty() || ids.len() > 100 {
      return Ok(invalid("ids needs between 1 and 100 post ids."));
   }
   match state.api.tweets_by_ids(&ids).await {
      Ok(body) => {
         let posts = crate::api::parse_tweets_by_ids(&body, &ids);
         helpers::remember_tweets(&state, &posts);
         Ok(json_ok(&page(
            posts.iter().map(|tweet| post(tweet, &tweets, &users, false)).collect(),
            None,
            None,
         )))
      },
      Err(err) => Ok(fail(&err)),
   }
}

#[derive(Clone, Copy)]
enum Graph {
   Following,
   Ids,
   Followers,
   Friends,
}

async fn user_graph(
   state: &AppState,
   jar: &CookieJar,
   username: &str,
   params: &PageParams,
   kind: Graph,
) -> Result<Response> {
   let (tweets_unused, users, cursor) = match page_fields(state, jar, params) {
      Ok(ready) => ready,
      Err(response) => return Ok(response),
   };
   let _ = tweets_unused;
   let Ok(name) = handle(username) else {
      return Ok(invalid("username is not a valid X handle."));
   };
   let user = match helpers::get_cached_user(state, name).await {
      Ok(user) => user,
      Err(err) => return Ok(fail(&err)),
   };
   if user.id.is_empty() {
      return Ok(fail(&Error::UserNotFound(name.to_owned())));
   }
   let count = match kind {
      Graph::Ids => match bounded_count(params.count, 5_000, 5_000) {
         Ok(count) => count,
         Err(response) => return Ok(response),
      },
      Graph::Followers | Graph::Friends => match bounded_count(params.count, 20, 200) {
         Ok(count) => count,
         Err(response) => return Ok(response),
      },
      Graph::Following => 20,
   };
   let key = format!(
      "g:{}:{}:{count}:{}",
      match kind {
         Graph::Following => "following",
         Graph::Ids => "ids",
         Graph::Followers => "followers",
         Graph::Friends => "friends",
      },
      user.id,
      cursor.unwrap_or("")
   );
   let api = state.clone();
   let user_id = user.id.clone();
   let page_cursor = cursor.map(str::to_owned);
   match kind {
      Graph::Ids => {
         match cached(state, &key, async move {
            api.api
               .friend_ids(&user_id, page_cursor.as_deref(), count)
               .await
         })
         .await
         {
            Ok((ids, next)) => Ok(json_ok(&page(ids, next, None))),
            Err(err) => Ok(fail(&err)),
         }
      },
      Graph::Following => {
         match cached(state, &key, async move {
            api.api.get_following(&user_id, page_cursor.as_deref()).await
         })
         .await
         {
            Ok(result) => Ok(users_body(state, result, &users)),
            Err(err) => Ok(fail(&err)),
         }
      },
      Graph::Followers => {
         match cached(state, &key, async move {
            api.api
               .followers_list(&user_id, page_cursor.as_deref(), count)
               .await
         })
         .await
         {
            Ok(result) => Ok(users_body(state, result, &users)),
            Err(err) => Ok(fail(&err)),
         }
      },
      Graph::Friends => {
         match cached(state, &key, async move {
            api.api
               .friends_list(&user_id, page_cursor.as_deref(), count)
               .await
         })
         .await
         {
            Ok(result) => Ok(users_body(state, result, &users)),
            Err(err) => Ok(fail(&err)),
         }
      },
   }
}

fn users_body(state: &AppState, result: PaginatedResult<User>, fields: &UserFields) -> Response {
   for user in &result.content {
      helpers::remember_user_id(state, user);
   }
   json_ok(&page(
      result.content.iter().map(|user| account(user, fields)).collect(),
      result.bottom,
      None,
   ))
}

fn posts_body(
   state: &AppState,
   timeline: crate::types::Timeline,
   tweets: &TweetFields,
   users: &UserFields,
) -> Response {
   let next = timeline.bottom.clone();
   let posts = flatten_posts(timeline);
   helpers::remember_tweets(state, &posts);
   json_ok(&page(
      posts.iter().map(|tweet| post(tweet, tweets, users, false)).collect(),
      next,
      None,
   ))
}

fn page_fields<'a>(
   state: &AppState,
   jar: &CookieJar,
   params: &'a PageParams,
) -> std::result::Result<(TweetFields, UserFields, Option<&'a str>), Response> {
   admit(state, jar)?;
   let tweets = TweetFields::parse(params.tweet_fields.as_deref()).map_err(invalid)?;
   let users = UserFields::parse(params.user_fields.as_deref(), UserFields::directory())
      .map_err(invalid)?;
   let cursor = cursor(params.cursor.as_deref())?;
   Ok((tweets, users, cursor))
}

fn bounded_count(raw: Option<u32>, default: u32, max: u32) -> std::result::Result<u32, Response> {
   let Some(count) = raw else {
      return Ok(default);
   };
   if (1..=max).contains(&count) {
      Ok(count)
   } else {
      Err(invalid(format!("count must be from 1 to {max}.")))
   }
}

fn snowflake_param(id: &str) -> Option<&str> {
   ((1..=19).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())).then_some(id)
}

async fn cached<T, F>(state: &AppState, key: &str, fetch: F) -> Result<T>
where
   T: Any + Clone + Send + Sync,
   F: std::future::Future<Output = Result<T>>,
{
   if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) = state.cache.lookup(key) {
      return Ok(hit);
   }
   let result = fetch.await;
   if let Ok(ref data) = result {
      state.cache.set_swr(key, data, ttl::DEFAULT, ttl::DEFAULT_STALE);
   }
   result
}

fn tab(filter: Option<&str>) -> std::result::Result<&'static str, Response> {
   match filter.map(str::trim) {
      None | Some("" | "latest" | "tweets" | "live") => Ok("tweets"),
      Some("top") => Ok("top"),
      Some("media") => Ok("media"),
      Some("users") => Ok("users"),
      Some("lists") => Ok("lists"),
      Some(_) => Err(invalid(
         "f must be latest, top, media, users, or lists.",
      )),
   }
}

fn check_query(params: &SearchQuery) -> std::result::Result<(), Response> {
   let query = params.query.as_deref().unwrap_or("").trim();
   if query.chars().count() > MAX_QUERY {
      return Err(invalid("q is longer than 512 characters."));
   }
   if query.bytes().any(|byte| byte.is_ascii_control()) {
      return Err(invalid("q is not a usable query."));
   }
   for (name, value, limit) in [
      ("from", params.from.as_deref(), 200),
      ("since", params.since.as_deref(), 40),
      ("until", params.until.as_deref(), 40),
      ("min_faves", params.min_faves.as_deref(), 12),
      ("filters", params.filters.as_deref(), MAX_LIST),
      ("exclude", params.exclude.as_deref(), MAX_LIST),
   ] {
      if value.is_some_and(|value| value.len() > limit || value.bytes().any(|byte| byte.is_ascii_control()))
      {
         return Err(invalid(format!("{name} is not a usable filter.")));
      }
   }
   Ok(())
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

fn cursor(value: Option<&str>) -> std::result::Result<Option<&str>, Response> {
   let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
      return Ok(None);
   };
   if value.len() > MAX_CURSOR || value.bytes().any(|byte| byte.is_ascii_control()) {
      return Err(invalid("cursor is not a page token from a previous response."));
   }
   Ok(Some(value))
}

fn handle(raw: &str) -> std::result::Result<&str, ()> {
   let name = raw.trim().trim_start_matches('@');
   if (1..=15).contains(&name.len())
      && name
         .chars()
         .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
   {
      Ok(name)
   } else {
      Err(())
   }
}

fn admit(state: &AppState, jar: &CookieJar) -> std::result::Result<(), Response> {
   if !state.passes.admits(jar) {
      return Err(json_err(
         StatusCode::UNAUTHORIZED,
         "pass_required",
         "A search pass is required. Create an API key at /search-pass, sign in, or whitelist \
          this address.",
      ));
   }
   Ok(())
}

fn invalid(message: impl Into<String>) -> Response {
   json_err(StatusCode::BAD_REQUEST, "invalid", message)
}

fn fail(err: &Error) -> Response {
   let (status, _, message) = err.presentation();
   let code = match err {
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
   json_err(status, code, message)
}

fn json_ok(body: &impl Serialize) -> Response {
   json_body(StatusCode::OK, body)
}

fn json_err(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
   json_body(
      status,
      &Failure {
         errors: [FailureItem {
            code,
            message: message.into(),
         }],
      },
   )
}

fn json_body(status: StatusCode, body: &impl Serialize) -> Response {
   let bytes = serde_json::to_vec(body).unwrap_or_else(|_| {
      br#"{"errors":[{"code":"error","message":"Could not encode the response."}]}"#.to_vec()
   });
   (
      status,
      [
         (
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
         ),
         (
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
         ),
      ],
      bytes,
   )
      .into_response()
}

#[derive(Serialize)]
struct Failure {
   errors: [FailureItem; 1],
}

#[derive(Serialize)]
struct FailureItem {
   code:    &'static str,
   message: String,
}

fn page<T: Serialize>(data: Vec<T>, cursor: Option<String>, query: Option<String>) -> Page<T> {
   let cursor = cursor.filter(|cursor| !cursor.is_empty());
   let result_count = data.len();
   Page {
      data,
      meta: Meta {
         result_count,
         cursor,
         query,
      },
   }
}

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

async fn load_timeline(
   state: &AppState,
   query: &str,
   product: &'static str,
   cursor: Option<&str>,
) -> Result<crate::types::Timeline> {
   if let Some(cursor) = cursor {
      let key = cache_keys::search_timeline(query, product, Some(cursor));
      if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) = state.cache.lookup(&key) {
         return Ok(hit);
      }
      let result = state.api.search(query, Some(cursor), product).await;
      if let Ok(ref data) = result {
         state
            .cache
            .set_swr(&key, data, ttl::SEARCH_CURSOR, ttl::SEARCH_CURSOR_STALE);
      }
      return result;
   }
   let key = cache_keys::search_timeline(query, product, None);
   cached_first(&state, &key, ttl::SEARCH, ttl::SEARCH_STALE, {
      let query = query.to_owned();
      move |state| {
         let query = query.clone();
         async move { state.api.search(&query, None, product).await }
      }
   })
   .await
}

async fn load_users(
   state: &AppState,
   query: &str,
   cursor: Option<&str>,
) -> Result<PaginatedResult<User>> {
   if let Some(cursor) = cursor {
      let key = cache_keys::search_users(query, Some(cursor));
      if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) = state.cache.lookup(&key) {
         return Ok(hit);
      }
      let result = state.api.search_users(query, Some(cursor)).await;
      if let Ok(ref data) = result {
         state
            .cache
            .set_swr(&key, data, ttl::SEARCH_CURSOR, ttl::SEARCH_CURSOR_STALE);
      }
      return result;
   }
   let key = cache_keys::search_users(query, None);
   cached_first(&state, &key, ttl::SEARCH, ttl::SEARCH_STALE, {
      let query = query.to_owned();
      move |state| {
         let query = query.clone();
         async move { state.api.search_users(&query, None).await }
      }
   })
   .await
}

async fn load_lists(
   state: &AppState,
   query: &str,
   cursor: Option<&str>,
) -> Result<PaginatedResult<List>> {
   if let Some(cursor) = cursor {
      let key = cache_keys::search_lists(query, Some(cursor));
      if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) = state.cache.lookup(&key) {
         return Ok(hit);
      }
      let result = state.api.search_lists(query, Some(cursor)).await;
      if let Ok(ref data) = result {
         state
            .cache
            .set_swr(&key, data, ttl::SEARCH_CURSOR, ttl::SEARCH_CURSOR_STALE);
      }
      return result;
   }
   let key = cache_keys::search_lists(query, None);
   cached_first(&state, &key, ttl::SEARCH, ttl::SEARCH_STALE, {
      let query = query.to_owned();
      move |state| {
         let query = query.clone();
         async move { state.api.search_lists(&query, None).await }
      }
   })
   .await
}

async fn load_user_posts(
   state: &AppState,
   username: &str,
   user_id: &str,
   cursor: Option<&str>,
) -> Result<crate::types::Timeline> {
   let key = cache_keys::filtered_posts(username, "all", "recent", cursor);
   if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) = state.cache.lookup(&key) {
      return Ok(hit);
   }
   let result = state
      .api
      .get_filtered_posts(user_id, cursor, "all", false)
      .await;
   if let Ok(ref data) = result {
      state
         .cache
         .set_swr(&key, data, ttl::DEFAULT, ttl::DEFAULT_STALE);
   }
   result
}

async fn cached_first<T, F, Fut>(
   state: &AppState,
   key: &str,
   fresh: u64,
   stale: u64,
   fetch: F,
) -> Result<T>
where
   T: Any + Clone + Send + Sync,
   F: Fn(AppState) -> Fut + Clone + Send + 'static,
   Fut: Future<Output = Result<T>> + Send,
{
   if let Some(hit) = helpers::swr_take(state, key, {
      let key = key.to_owned();
      let fetch = fetch.clone();
      move |state| async move {
         if let Ok(data) = fetch(state.clone()).await {
            state.cache.set_swr(&key, &data, fresh, stale);
         }
      }
   }) {
      return Ok(hit);
   }
   let result = fetch(state.clone()).await;
   if let Ok(ref data) = result {
      state.cache.set_swr(key, data, fresh, stale);
   }
   result
}

fn flatten_posts(timeline: crate::types::Timeline) -> Vec<Tweet> {
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
         card:     true,
         poll:     true,
         source:   true,
         location: true,
         note:     true,
         history:  true,
         variants: true,
         labels:   true,
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

   fn parse(raw: Option<&str>) -> std::result::Result<Self, String> {
      let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
         return Ok(Self::compact());
      };
      let tokens = raw
         .split(',')
         .map(str::trim)
         .filter(|token| !token.is_empty())
         .collect::<Vec<_>>();
      if tokens.iter().any(|token| *token == "all") {
         if tokens.iter().any(|token| *token != "all") {
            return Err("tweet.fields all cannot be combined with other names.".into());
         }
         return Ok(Self::all());
      }
      let mut fields = Self::none();
      for token in tokens {
         if !fields.enable(token) && token != "id" && token != "text" {
            return Err(format!("Unknown tweet field \"{token}\"."));
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
         quote:     false,
         entities:  false,
         card:      false,
         poll:      false,
         source:    false,
         location:  false,
         note:      false,
         history:   false,
         variants:  false,
         labels:    false,
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
         verified:  true,
         avatar:    true,
         bio:       true,
         location:  true,
         website:   true,
         followers: true,
         following: true,
         posts:     true,
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

   fn parse(raw: Option<&str>, default: Self) -> std::result::Result<Self, String> {
      let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
         return Ok(default);
      };
      let tokens = raw
         .split(',')
         .map(str::trim)
         .filter(|token| !token.is_empty())
         .collect::<Vec<_>>();
      if tokens.iter().any(|token| *token == "all") {
         if tokens.iter().any(|token| *token != "all") {
            return Err("user.fields all cannot be combined with other names.".into());
         }
         return Ok(Self::full());
      }
      let mut fields = Self::stub();
      for token in tokens {
         if !fields.enable(token) && !matches!(token, "id" | "username" | "name") {
            return Err(format!("Unknown user field \"{token}\"."));
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
   id:         String,
   username:   String,
   #[serde(skip_serializing_if = "String::is_empty")]
   name:       String,
   #[serde(skip_serializing_if = "Option::is_none")]
   verified:   Option<&'static str>,
   #[serde(skip_serializing_if = "Option::is_none")]
   avatar_url: Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   banner_url: Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   bio:        Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   location:   Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   website:    Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   followers:  Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   following:  Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   posts:      Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   likes:      Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   media:      Option<i64>,
   #[serde(skip_serializing_if = "Option::is_none")]
   joined_at:  Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   pinned_tweet_id: Option<String>,
   #[serde(skip_serializing_if = "is_false")]
   protected:  bool,
   #[serde(skip_serializing_if = "is_false")]
   suspended:  bool,
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
      banner_url: fields.banner.then(|| link(&user.banner)).flatten(),
      bio: fields.bio.then(|| {
         let bio = views::layout::strip_html(&user.bio);
         (!bio.trim().is_empty()).then_some(bio)
      }).flatten(),
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
   id:          String,
   text:        String,
   #[serde(skip_serializing_if = "Option::is_none")]
   created_at:  Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   lang:        Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   author:      Option<Account>,
   #[serde(skip_serializing_if = "Option::is_none")]
   metrics:     Option<Metrics>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   reply_to:    Vec<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   in_reply_to_status_id: Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   conversation_id: Option<String>,
   #[serde(skip_serializing_if = "is_false")]
   sensitive:   bool,
   #[serde(skip_serializing_if = "Option::is_none")]
   unavailable: Option<String>,
   #[serde(skip_serializing_if = "is_false")]
   pinned:      bool,
   #[serde(skip_serializing_if = "Option::is_none")]
   reposted_by: Option<Account>,
   #[serde(skip_serializing_if = "Option::is_none")]
   repost_id:   Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   quote:       Option<Box<Post>>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   media:       Vec<Media>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   entities:    Vec<Ent>,
   #[serde(skip_serializing_if = "Option::is_none")]
   card:        Option<LinkCard>,
   #[serde(skip_serializing_if = "Option::is_none")]
   poll:        Option<PollOut>,
   #[serde(skip_serializing_if = "Option::is_none")]
   source:      Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   location:    Option<String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   note:        Option<String>,
   #[serde(skip_serializing_if = "Vec::is_empty")]
   history:     Vec<String>,
   #[serde(skip_serializing_if = "is_false")]
   paid_promotion: bool,
   #[serde(skip_serializing_if = "is_false")]
   ai_generated: bool,
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
      id: snowflake(shown.id).unwrap_or_else(|| "0".to_owned()),
      text: shown.text.clone(),
      created_at: fields.created_at.then(|| stamp(shown.time)).flatten(),
      lang: fields.lang.then(|| nonempty(&shown.lang)).flatten(),
      author: fields.author.then(|| account(&shown.user, users)),
      metrics: fields.metrics.then(|| metrics(&shown.stats)).flatten(),
      reply_to: fields
         .reply
         .then(|| shown.reply.clone())
         .unwrap_or_default(),
      in_reply_to_status_id: fields.reply.then(|| snowflake(shown.reply_id)).flatten(),
      conversation_id: fields
         .reply
         .then(|| {
            (shown.thread_id != 0 && shown.thread_id != shown.id).then(|| snowflake(shown.thread_id))
         })
         .flatten()
         .flatten(),
      sensitive: fields.sensitive && shown.sensitive,
      unavailable: (!shown.available).then(|| {
         if shown.tombstone.is_empty() {
            "unavailable".to_owned()
         } else {
            shown.tombstone.clone()
         }
      }),
      pinned: shown.pinned,
      reposted_by: repost
         .filter(|wrapper| !wrapper.user.username.is_empty())
         .map(|wrapper| account(&wrapper.user, users)),
      repost_id: repost
         .filter(|wrapper| wrapper.id != 0 && wrapper.id != shown.id)
         .and_then(|wrapper| snowflake(wrapper.id)),
      quote: fields.quote.then(|| {
         shown
            .quote
            .as_deref()
            .filter(|quote| quote.id != 0 || !quote.text.is_empty())
            .map(|quote| Box::new(post(quote, &fields, users, true)))
      }).flatten(),
      media: fields
         .media
         .then(|| media_of(shown, fields.variants))
         .unwrap_or_default(),
      entities: fields
         .entities
         .then(|| shown.entities.iter().map(ent).collect())
         .unwrap_or_default(),
      card: fields.card.then(|| shown.card.as_ref().and_then(card_of)).flatten(),
      poll: fields.poll.then(|| shown.poll.as_ref().and_then(poll_of)).flatten(),
      source: fields.source.then(|| nonempty(&shown.source)).flatten(),
      location: fields.location.then(|| nonempty(&shown.location)).flatten(),
      note: fields
         .note
         .then(|| shown.note.as_ref().and_then(|note| nonempty(&note.text)))
         .flatten(),
      history: fields
         .history
         .then(|| history_of(shown))
         .unwrap_or_default(),
      paid_promotion: fields.labels && shown.paid_promotion,
      ai_generated: fields.labels && shown.ai_generated,
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
      .and_then(link_str)
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

fn video_size(video: &Video) -> (Option<i32>, Option<i32>) {
   let url = video.best_mp4_url().unwrap_or(video.url.as_str());
   if let Some((width, height)) = size_from_url(url) {
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

fn size_from_url(url: &str) -> Option<(i32, i32)> {
   let after = url.split("/vid/").nth(1)?;
   for segment in after.split('/') {
      let dims = segment
         .split_once('?')
         .map_or(segment, |(before, _)| before);
      let Some((width, height)) = dims.split_once('x') else {
         continue;
      };
      let Ok(width) = width.parse::<i32>() else {
         continue;
      };
      let Ok(height) = height.parse::<i32>() else {
         continue;
      };
      if width > 0 && height > 0 {
         return Some((width, height));
      }
   }
   None
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
      kind: match entity.kind {
         EntityKind::Url => "url",
         EntityKind::Mention => "mention",
         EntityKind::Hashtag => "hashtag",
         EntityKind::Symbol => "symbol",
      },
      start: entity.indices.0,
      end: entity.indices.1,
      text: entity.display.clone(),
      url: entity.url.clone(),
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
         .map(|(index, label)| Choice {
            label: label.clone(),
            votes: poll.values.get(index).copied().unwrap_or(0),
         })
         .collect(),
      votes: poll.votes,
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

fn link_str(url: &str) -> Option<String> {
   link(url)
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
      assert!(UserFields::parse(Some("avatar"), UserFields::stub()).unwrap().avatar);
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
               url: "https://video.twimg.com/vid/1280x720/clip.mp4".into(),
               bitrate: 1,
               resolution: 720,
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
   fn error_body_is_one_list() {
      let body = Failure {
         errors: [FailureItem {
            code: "pass_required",
            message: "A search pass is required.".into(),
         }],
      };
      let value = serde_json::to_value(body).unwrap();
      assert_eq!(value["errors"][0]["code"], "pass_required");
   }

   #[test]
   fn str_skip_helper_compiles_for_lists() {
      let row = ListRow {
         id: "1",
         name: "name",
         description: "",
         username: "",
         members: 0,
         subscribers: 2,
      };
      let value = serde_json::to_value(row).unwrap();
      assert!(value.get("description").is_none());
      assert_eq!(value["subscribers"], 2);
   }
}
