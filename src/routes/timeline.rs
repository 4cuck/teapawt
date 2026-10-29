use std::cmp;

use axum::{
   Router,
   extract::{Path, Query, State},
   response::{Html, IntoResponse as _, Response},
   routing::get,
};
use axum_extra::extract::CookieJar;
use maud::{PreEscaped, html};
use serde::Deserialize;

use super::{
   helpers,
   helpers::extract_timeline,
};
use crate::{
   AppState,
   api::budget,
   cache::{Hit, keys as cache_keys, ttl},
   error::{Error, Result},
   types::{Prefs, Profile, Timeline, TimelineKind},
   utils::formatters,
   views::{layout, profile, search::render_search_panel_with_action, timeline},
};

/// Below this many accounts with `UserTweets` left, a stale profile is shown
/// as-is. The background refresh would spend the quota the visitor needs.
const TIMELINE_REFRESH_FLOOR: usize = 8;

fn profile_description(user: &crate::types::User, host: &str) -> String {
   let who = if user.fullname.trim().is_empty() {
      format!("@{}", user.username)
   } else {
      format!("{} (@{})", user.fullname.trim(), user.username)
   };
   let bio = layout::strip_html(&user.bio);
   let bio = bio.split_whitespace().collect::<Vec<_>>().join(" ");
   let lead = format!("{who} on {host}.");
   if bio.is_empty() {
      return format!("{lead} Read their latest posts without an X account.");
   }
   let desc = format!("{lead} {bio}");
   if desc.chars().count() > 160 {
      format!("{}…", desc.chars().take(159).collect::<String>())
   } else {
      desc
   }
}

fn profile_json_ld(user: &crate::types::User, page_url: &str, image: &str) -> maud::Markup {
   let name = if user.fullname.trim().is_empty() {
      format!("@{}", user.username)
   } else {
      user.fullname.trim().to_owned()
   };
   let bio = layout::strip_html(&user.bio);
   let mut entity = serde_json::json!({
      "@type": "Person",
      "name": name,
      "alternateName": format!("@{}", user.username),
      "identifier": user.username,
      "url": page_url,
      "description": bio,
      "sameAs": format!("https://x.com/{}", user.username),
      "interactionStatistic": [
         {
            "@type": "InteractionCounter",
            "interactionType": "https://schema.org/FollowAction",
            "userInteractionCount": user.followers.max(0)
         },
         {
            "@type": "InteractionCounter",
            "interactionType": "https://schema.org/WriteAction",
            "userInteractionCount": user.tweets.max(0)
         }
      ]
   });
   if !image.is_empty() {
      entity["image"] = serde_json::Value::String(image.to_owned());
   }
   let value = serde_json::json!({
      "@context": "https://schema.org",
      "@type": "ProfilePage",
      "url": page_url,
      "mainEntity": entity
   });
   let json = value.to_string().replace('<', "\\u003c");
   html! {
       script type="application/ld+json" { (PreEscaped(json)) }
   }
}

async fn load_first_profile(state: &AppState, username: &str) -> Result<Profile> {
   let cache_key = cache_keys::profile(username);
   match state.cache.lookup::<Profile>(&cache_key) {
      Some(Hit::Fresh(profile)) => {
         tracing::debug!("Cache hit for profile: {username}");
         return Ok(profile);
      },
      Some(Hit::Stale(profile)) => {
         if state.api.user_timeline_accounts().await >= TIMELINE_REFRESH_FLOOR
            && let Some(guard) = state.cache.start_refresh(&cache_key)
         {
            let state = state.clone();
            let username = username.to_owned();
            tokio::spawn(async move {
               let _guard = guard;
               refresh_profile(&state, &username).await;
            });
         }
         tracing::debug!("Stale profile for {username}");
         return Ok(profile);
      },
      None => {},
   }

   match fetch_profile(state, username).await {
      Ok(profile) => Ok(profile),
      Err(err) if err.is_upstream_limit() => state.cache.recall(&cache_key).ok_or(err),
      Err(err) => Err(err),
   }
}

async fn refresh_profile(state: &AppState, username: &str) {
   let hint = helpers::user_hint(state, username);
   let rail = helpers::cached_photo_rail(state, username);
   if let Ok(open) = state
      .api
      .get_profile_hinted(username, None, hint.as_ref(), rail)
      .await
   {
      helpers::publish_profile(state, open);
   }
}

async fn fetch_profile(state: &AppState, username: &str) -> Result<Profile> {
   let hint = helpers::user_hint(state, username);
   let rail = helpers::cached_photo_rail(state, username);
   let mut open = state
      .api
      .get_profile_hinted(username, None, hint.as_ref(), rail)
      .await?;
   // About Account is a separate X call and is the one that sits on a dead
   // connection. The timeline does not need it; a cached copy is applied and
   // a miss is filled in the background.
   helpers::apply_cached_account_context(state, &mut open.profile.user);
   Ok(helpers::publish_profile(state, open))
}

/// A cursor page is reused for a few minutes, and the last copy is served
/// when `UserTweets` is exhausted.
async fn load_profile_cursor(state: &AppState, username: &str, cursor: &str) -> Result<Profile> {
   let cache_key = cache_keys::profile_cursor(username, cursor);
   if let Some(Hit::Fresh(profile) | Hit::Stale(profile)) = state.cache.lookup(&cache_key) {
      return Ok(profile);
   }

   let hint = helpers::user_hint(state, username);
   match state
      .api
      .get_profile_hinted(username, Some(cursor), hint.as_ref(), None)
      .await
   {
      Ok(open) => {
         let profile = open.into_profile();
         state.cache.set_swr(
            &cache_key,
            &profile,
            ttl::PROFILE_CURSOR,
            ttl::PROFILE_CURSOR_STALE,
         );
         Ok(profile)
      },
      Err(err) if err.is_upstream_limit() => state.cache.recall(&cache_key).ok_or(err),
      Err(err) => Err(err),
   }
}

/// Reserved path names that cannot be usernames.
const RESERVED_PATHS: &[&str] = &[
   "pic",
   "gif",
   "video",
   "search",
   "settings",
   "login",
   "intent",
   "i",
   "about",
   "explore",
   "help",
   "oauth",
   "oauth2",
   "saveprefs",
   "resetprefs",
   "enablemp4",
   "enablehls",
   "public",
   "css",
   "js",
   "fonts",
   "embed",
];

/// Validate username contains only valid characters.
fn is_valid_username(name: &str) -> bool {
   !name.is_empty()
      && name
         .chars()
         .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ',')
      && !name.contains('.')
}

/// Check if a username is actually a reserved path.
fn is_reserved_path(name: &str) -> bool {
   let lower = name.to_lowercase();
   RESERVED_PATHS.iter().any(|&reserved| reserved == lower)
}

/// Split comma-separated usernames and validate each.
fn parse_usernames(name: &str) -> Option<Vec<String>> {
   let names = name
      .split(',')
      .map(|part| part.trim().to_owned())
      .filter(|part| !part.is_empty())
      .collect::<Vec<_>>();

   if names.is_empty()
      || names
         .iter()
         .any(|n| !is_valid_username(n) || is_reserved_path(n))
   {
      return None;
   }

   Some(names)
}

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
   pub tab: Option<String>,
   pub cursor: Option<String>,
   pub view: Option<String>,
   /// For AJAX infinite scroll requests - returns only tweet HTML.
   pub scroll: Option<String>,
   /// Posts menu: `all`, `posts`, or `highlights`.
   pub filter: Option<String>,
   /// Posts menu sort: `recent` or `popular`.
   pub sort: Option<String>,
}

fn posts_filter(raw: Option<&str>) -> &'static str {
   match raw.unwrap_or("").trim() {
      "posts" | "originals" => "posts",
      "highlights" | "highlight" => "highlights",
      _ => "all",
   }
}

fn posts_sort(raw: Option<&str>) -> &'static str {
   match raw.unwrap_or("").trim() {
      "popular" | "likes" => "popular",
      _ => "recent",
   }
}

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/{username}", get(user_timeline))
      .route("/{username}/", get(user_timeline))
      .route("/{username}/with_replies", get(user_replies))
      .route("/{username}/reposts", get(user_reposts))
      .route("/{username}/articles", get(user_articles))
      .route("/{username}/media", get(user_media))
      .route("/{username}/search", get(user_search))
}

async fn filtered_posts(
   state: &AppState,
   prefs: &Prefs,
   username: &str,
   cursor: Option<&str>,
   filter: &'static str,
   sort: &'static str,
   is_scroll: bool,
) -> Result<Response> {
   let (user, (timeline_result, photo_rail)) =
      helpers::with_user(state, username, |user_id| async move {
         let posts = filtered_timeline(state, username, &user_id, cursor, filter, sort);
         if is_scroll {
            (posts.await, Vec::new())
         } else {
            helpers::beside_rail(posts, helpers::photo_rail(state, username, &user_id)).await
         }
      })
      .await?;

   match timeline_result {
      Ok(data) => {
         let (mut tweets, next_cursor) = extract_timeline(data);
         helpers::enrich_tweet_groups(state, &mut tweets).await;
         for group in &tweets {
            helpers::remember_tweets(state, group);
         }
         let base_url = timeline::posts_href(&user.username, filter, sort);
         if is_scroll {
            let content = timeline::render_timeline_with_prefs(
               &tweets,
               &state.config,
               next_cursor.as_deref(),
               Some(&base_url),
               prefs,
               None,
            );
            return Ok(Html(content.into_string()).into_response());
         }
         let show_articles = helpers::user_has_articles(state, &user.id).await;
         let newer = cursor.is_some().then_some(base_url.as_str());
         let timeline_content = html! {
             (timeline::render_timeline_with_prefs(
                &tweets,
                &state.config,
                next_cursor.as_deref(),
                Some(&base_url),
                prefs,
                newer,
             ))
         };
         let menu = timeline::PostsMenu {
            filter,
            sort,
            show_highlights: user.has_highlights || filter == "highlights",
            show_articles,
         };
         let content = profile::render_profile_page(
            &user,
            &photo_rail,
            &state.config,
            prefs,
            TimelineKind::Tweets,
            &timeline_content,
            "",
            &menu,
         );
         let title = match filter {
            "highlights" => format!("Highlights from @{}", user.username),
            "posts" => format!("Posts from @{}", user.username),
            _ => format!("{} (@{})", user.fullname, user.username),
         };
         let markup = layout::PageLayout::new(&state.config, &title, content)
            .prefs(prefs)
            .referer(&base_url)
            .render();
         Ok(Html(markup.into_string()).into_response())
      },
      Err(err) => Ok(helpers::api_error(&state.config, &err)),
   }
}

/// One page of the posts menu, from cache when it has one.
async fn filtered_timeline(
   state: &AppState,
   username: &str,
   user_id: &str,
   cursor: Option<&str>,
   filter: &'static str,
   sort: &'static str,
) -> Result<Timeline> {
   let cache_key = cache_keys::filtered_posts(username, filter, sort, cursor);
   if let Some(Hit::Fresh(hit) | Hit::Stale(hit)) = state.cache.lookup::<Timeline>(&cache_key) {
      return Ok(hit);
   }
   let result = state
      .api
      .get_filtered_posts(user_id, cursor, filter, sort == "popular")
      .await;
   if let Ok(ref data) = result {
      state
         .cache
         .set_swr(&cache_key, data, ttl::DEFAULT, ttl::DEFAULT_STALE);
   }
   result
}

async fn user_timeline(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<TimelineQuery>,
) -> Result<Response> {
   // Validate username
   if is_reserved_path(&username) {
      return Err(Error::InvalidUrl(format!(
         "'{username}' is a reserved path",
      )));
   }

   // Check for multi-user search (comma-separated)
   if username.contains(',') {
      return multi_user_timeline(state, jar, username, query).await;
   }

   if !is_valid_username(&username) {
      return Err(Error::InvalidUrl("Invalid username format".into()));
   }

   // Get the tab type and validate
   let tab = query.tab.as_deref().unwrap_or("tweets");
   if !["tweets", "with_replies", "media", "search", ""].contains(&tab) {
      return Err(Error::InvalidUrl("Invalid tab parameter".into()));
   }

   // Check if this is an AJAX scroll request
   let is_scroll = query.scroll.as_ref().is_some_and(|val| val == "true");
   let filter = posts_filter(query.filter.as_deref());
   let sort = posts_sort(query.sort.as_deref());

   // Extract prefs from cookies
   let prefs = Prefs::from_cookies(&jar, &state.config);
   if filter != "all" || sort != "recent" {
      return filtered_posts(
         &state,
         &prefs,
         &username,
         query.cursor.as_deref(),
         filter,
         sort,
         is_scroll,
      )
      .await;
   }

   let profile_result = if let Some(cursor) = query.cursor.as_deref() {
      load_profile_cursor(&state, &username, cursor).await
   } else {
      load_first_profile(&state, &username).await
   };

   match profile_result {
      Ok(mut profile_data) => {
         helpers::apply_cached_account_context(&state, &mut profile_data.user);
         // For AJAX scroll requests, return only the tweets HTML
         if is_scroll {
            let (mut tweets, cursor) = extract_timeline(profile_data.tweets);
            helpers::enrich_tweet_groups(&state, &mut tweets).await;
            for group in &tweets {
               helpers::remember_tweets(&state, group);
            }
            let base_url = format!("/{}", profile_data.user.username);
            let content = timeline::render_timeline_with_prefs(
               &tweets,
               &state.config,
               cursor.as_deref(),
               Some(&base_url),
               &prefs,
               None,
            );
            return Ok(Html(content.into_string()).into_response());
         }

         let origin = crate::utils::host::current_origin(state.config.url_prefix());
         let host = crate::utils::host::request_origin().map_or_else(
            || crate::utils::host::host_label(state.config.url_prefix()).to_owned(),
            |value| crate::utils::host::host_label(&value).to_owned(),
         );
         let title = if profile_data.user.fullname.trim().is_empty() {
            format!("@{}", profile_data.user.username)
         } else {
            format!(
               "{} (@{})",
               profile_data.user.fullname.trim(),
               profile_data.user.username
            )
         };
         let desc = profile_description(&profile_data.user, &host);
         let canonical = format!("https://x.com/{}", profile_data.user.username);
         let rss_url = format!("/{}/rss", profile_data.user.username);
         let referer = format!("/{}", profile_data.user.username);
         let page_url = format!("{origin}/{}", profile_data.user.username);
         let og_image = formatters::join_base(
            &origin,
            &formatters::get_pic_url(
               &profile_data.user.user_pic.replace("_normal", "_400x400"),
               state.config.config.base64_media,
               prefs.proxy_media,
            ),
         );
         let head = profile_json_ld(&profile_data.user, &page_url, &og_image);
         let newer = query
            .cursor
            .is_some()
            .then(|| format!("/{}", profile_data.user.username));
         for group in &profile_data.tweets.content {
            helpers::remember_tweets(&state, group);
         }
         if let Some(first) = profile_data.tweets.content.first() {
            helpers::prefetch_profiles(&state, first);
         }

         let show_articles = helpers::user_has_articles(&state, &profile_data.user.id).await;
         let menu = timeline::PostsMenu {
            filter: "all",
            sort: "recent",
            show_highlights: profile_data.user.has_highlights,
            show_articles,
         };
         let content = profile::render_profile_with_prefs(
            &profile_data,
            &state.config,
            tab,
            Some(&prefs),
            newer.as_deref(),
            &menu,
         );
         let markup = layout::PageLayout::new(&state.config, &title, content)
            .description(&desc)
            .prefs(&prefs)
            .rss(&rss_url)
            .canonical(&canonical)
            .referer(&referer)
            .og_image(&og_image)
            .og_type("object")
            .head_extra(&head)
            .render();

         Ok(Html(markup.into_string()).into_response())
      },
      Err(err) => Ok(helpers::api_error_titled(&state.config, &err, "Error")),
   }
}

/// Shared handler for user sub-tab timelines (replies, media).
async fn user_tab_handler(
   state: &AppState,
   prefs: &Prefs,
   username: &str,
   cursor: Option<&str>,
   tab: TimelineKind,
   include_photo_rail: bool,
   view: &str,
) -> Result<Response> {
   let (cache_kind, tab_str, title_prefix) = match tab {
      TimelineKind::Replies => ("replies", "with_replies", "Tweets & replies from"),
      TimelineKind::Reposts => ("reposts", "reposts", "Reposts from"),
      TimelineKind::Articles => ("articles", "articles", "Articles from"),
      TimelineKind::Media => ("media", "media", "Media from"),
      _ => unreachable!(),
   };

   let (user, (timeline_result, photo_rail)) =
      helpers::with_user(state, username, |user_id| async move {
         let timeline = tab_timeline(state, username, &user_id, cursor, tab, cache_kind);
         if include_photo_rail {
            helpers::beside_rail(timeline, helpers::photo_rail(state, username, &user_id)).await
         } else {
            (timeline.await, Vec::new())
         }
      })
      .await?;

   let has_request_cursor = cursor.is_some();

   match timeline_result {
      Ok(data) => {
         let (mut tweets, next_cursor) = extract_timeline(data);
         helpers::enrich_tweet_groups(state, &mut tweets).await;
         for group in &tweets {
            helpers::remember_tweets(state, group);
         }
         if let Some(first) = tweets.first() {
            helpers::prefetch_profiles(state, first);
         }
         let base_url = if tab == TimelineKind::Media && view != "timeline" && !view.is_empty() {
            format!("/{username}/{tab_str}?view={view}")
         } else {
            format!("/{username}/{tab_str}")
         };
         let newer = has_request_cursor.then_some(base_url.as_str());
         let media_base = format!("/{username}/{tab_str}");

         let timeline_content = html! {
             @if tab == TimelineKind::Media {
                 (timeline::render_media_view_tabs(&media_base, view))
             }
             (timeline::render_timeline_with_view(
                 &tweets,
                 &state.config,
                 next_cursor.as_deref(),
                 Some(&base_url),
                 prefs,
                 newer,
                 if tab == TimelineKind::Media { view } else { "" },
             ))
         };
         let show_articles = if tab == TimelineKind::Articles {
            let has = tweets.iter().flatten().next().is_some();
            state.cache.set_swr(
               &cache_keys::articles_flag(&user.id),
               &has,
               ttl::ARTICLES_FLAG,
               ttl::ARTICLES_FLAG_STALE,
            );
            has
         } else {
            helpers::user_has_articles(state, &user.id).await
         };
         let menu = timeline::PostsMenu {
            filter: "all",
            sort: "recent",
            show_highlights: user.has_highlights,
            show_articles,
         };
         let content = profile::render_profile_page(
            &user,
            &photo_rail,
            &state.config,
            prefs,
            tab,
            &timeline_content,
            view,
            &menu,
         );

         let title = format!("{title_prefix} @{}", user.username);
         let canonical = format!("https://x.com/{username}/{tab_str}");
         let rss_url = format!("/{username}/{tab_str}/rss");
         let referer = format!("/{username}/{tab_str}");
         let markup = layout::PageLayout::new(&state.config, &title, content)
            .prefs(prefs)
            .rss(&rss_url)
            .canonical(&canonical)
            .referer(&referer)
            .render();
         Ok(Html(markup.into_string()).into_response())
      },
      Err(err) => Ok(helpers::api_error(&state.config, &err)),
   }
}

/// A profile tab's timeline, with the cache for its first page.
async fn tab_timeline(
   state: &AppState,
   username: &str,
   user_id: &str,
   cursor: Option<&str>,
   tab: TimelineKind,
   cache_kind: &str,
) -> Result<Timeline> {
   if cursor.is_some() {
      return fetch_tab(state, user_id, cursor, tab).await;
   }
   let cache_key = cache_keys::timeline(username, cache_kind);
   if let Some(cached) = helpers::swr_take::<Timeline, _, _>(state, &cache_key, {
      let user_id = user_id.to_owned();
      let cache_key = cache_key.clone();
      move |state| async move {
         if let Ok(data) = fetch_tab(&state, &user_id, None, tab).await {
            state
               .cache
               .set_swr(&cache_key, &data, ttl::DEFAULT, ttl::DEFAULT_STALE);
         }
      }
   }) {
      return Ok(cached);
   }
   let result = fetch_tab(state, user_id, None, tab).await;
   if let Ok(ref data) = result {
      state
         .cache
         .set_swr(&cache_key, data, ttl::DEFAULT, ttl::DEFAULT_STALE);
   }
   result
}

async fn fetch_tab(
   state: &AppState,
   user_id: &str,
   cursor: Option<&str>,
   tab: TimelineKind,
) -> Result<Timeline> {
   match tab {
      TimelineKind::Replies => state.api.get_user_tweets_and_replies(user_id, cursor).await,
      TimelineKind::Reposts => state.api.get_user_reposts(user_id, cursor).await,
      TimelineKind::Articles => state.api.get_user_articles(user_id, cursor).await,
      TimelineKind::Media => state.api.get_user_media(user_id, cursor).await,
      _ => Err(Error::InvalidUrl("Invalid tab parameter".into())),
   }
}

async fn user_replies(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<TimelineQuery>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   user_tab_handler(
      &state,
      &prefs,
      &username,
      query.cursor.as_deref(),
      TimelineKind::Replies,
      true,
      "",
   )
   .await
}

async fn user_reposts(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<TimelineQuery>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   user_tab_handler(
      &state,
      &prefs,
      &username,
      query.cursor.as_deref(),
      TimelineKind::Reposts,
      true,
      "",
   )
   .await
}

async fn user_articles(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<TimelineQuery>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   user_tab_handler(
      &state,
      &prefs,
      &username,
      query.cursor.as_deref(),
      TimelineKind::Articles,
      true,
      "",
   )
   .await
}

async fn user_media(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<TimelineQuery>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let view = prefs.resolved_media_view(query.view.as_deref());
   user_tab_handler(
      &state,
      &prefs,
      &username,
      query.cursor.as_deref(),
      TimelineKind::Media,
      false,
      view,
   )
   .await
}

#[derive(Debug, serde::Deserialize)]
pub struct UserSearchQuery {
   #[serde(rename = "q")]
   pub query: Option<String>,
   pub cursor: Option<String>,
}

async fn user_search(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<UserSearchQuery>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);

   let search_query = query.query.as_deref().unwrap_or("");
   let search_action = format!("/{username}/search");

   // When search query is empty, default to showing the user's recent tweets
   let api_query = if search_query.is_empty() {
      format!("from:{username} include:nativeretweets")
   } else {
      format!("from:{username} {search_query}")
   };
   // The search names the account, not its id, so it never waits on the user.
   let (state_ref, name) = (&state, username.as_str());
   let (fetched, search_result) = tokio::join!(
      helpers::with_user(state_ref, name, |user_id| async move {
         helpers::photo_rail(state_ref, name, &user_id).await
      }),
      state
         .api
         .search(&api_query, query.cursor.as_deref(), "Latest"),
   );
   let (user, photo_rail) = fetched?;

   match search_result {
      Ok(results) => {
         let (mut tweets, cursor) = extract_timeline(results);
         helpers::enrich_tweet_groups(&state, &mut tweets).await;
         let base_url = if search_query.is_empty() {
            format!("/{username}/search")
         } else {
            format!(
               "/{}/search?q={}",
               username,
               percent_encoding::utf8_percent_encode(
                  search_query,
                  percent_encoding::NON_ALPHANUMERIC,
               )
            )
         };

         let newer = query.cursor.is_some().then_some(base_url.as_str());
         let timeline_content = html! {
             div class="timeline-header" {
                 (render_search_panel_with_action(search_query, None, &search_action, "tweets", ""))
             }
             (timeline::render_timeline_with_prefs(&tweets, &state.config, cursor.as_deref(), Some(&base_url), &prefs, newer))
         };
         let show_articles = helpers::user_has_articles(&state, &user.id).await;
         let menu = timeline::PostsMenu {
            filter: "all",
            sort: "recent",
            show_highlights: user.has_highlights,
            show_articles,
         };
         let content = profile::render_profile_page(
            &user,
            &photo_rail,
            &state.config,
            &prefs,
            TimelineKind::Search,
            &timeline_content,
            "",
            &menu,
         );

         let title = if search_query.is_empty() {
            format!("Search @{username}")
         } else {
            format!("Search @{username}: {search_query}")
         };
         let canonical = format!("https://x.com/{username}");
         let rss_url = format!("/{username}/search/rss?f=tweets");
         let referer = format!("/{username}/search");
         let markup = layout::PageLayout::new(&state.config, &title, content)
            .prefs(&prefs)
            .rss(&rss_url)
            .canonical(&canonical)
            .referer(&referer)
            .render();
         Ok(Html(markup.into_string()).into_response())
      },
      Err(err) => Ok(helpers::api_error_titled(
         &state.config,
         &err,
         "Search Error",
      )),
   }
}

/// Fetch photo rail for a user, returning empty vec on error.
/// Handle multi-user timeline (comma-separated usernames).
async fn multi_user_timeline(
   state: AppState,
   jar: CookieJar,
   usernames_str: String,
   query: TimelineQuery,
) -> Result<Response> {
   const PAGE_SIZE: usize = 20;

   let prefs = Prefs::from_cookies(&jar, &state.config);

   let Some(usernames) = parse_usernames(&usernames_str) else {
      return Err(Error::InvalidUrl(
         "Invalid username format in multi-user search".into(),
      ));
   };

   if usernames.len() > 10 {
      return Err(Error::InvalidUrl(
         "Maximum 10 users allowed in multi-user search".into(),
      ));
   }

   let cursor_id = query
      .cursor
      .as_ref()
      .and_then(|val| val.parse::<i64>().ok());

   let mut all_tweets = Vec::new();
   let handles: Vec<_> = usernames
      .iter()
      .map(|username| {
         let state = state.clone();
         let username = username.clone();
         // A spawned task starts with an empty task-local scope, so the caller
         // has to be carried over or these fan-out calls go unbilled.
         tokio::spawn(budget::scoped(async move {
            let api = &state.api;
            let (_, timeline) = helpers::with_user(&state, &username, |user_id| async move {
               api.get_user_tweets(&user_id, None).await
            })
            .await
            .ok()?;
            Some(
               timeline
                  .ok()?
                  .content
                  .into_iter()
                  .flatten()
                  .collect::<Vec<_>>(),
            )
         }))
      })
      .collect();

   for handle in handles {
      if let Ok(Some(tweets)) = handle.await {
         all_tweets.extend(tweets);
      }
   }

   all_tweets.sort_by_key(|tweet| cmp::Reverse(tweet.time));

   if let Some(cursor) = cursor_id {
      all_tweets.retain(|tweet| tweet.id < cursor);
   }

   let has_more = all_tweets.len() > PAGE_SIZE;
   all_tweets.truncate(PAGE_SIZE);

   let next_cursor = if has_more {
      all_tweets.last().map(|tweet| tweet.id.to_string())
   } else {
      None
   };

   let title = format!("Tweets from {}", usernames.join(", "));
   let base_url = format!("/{usernames_str}");

   let newer = query.cursor.is_some().then_some(base_url.as_str());
   let mut groups = all_tweets
      .into_iter()
      .map(|tweet| vec![tweet])
      .collect::<Vec<Vec<_>>>();
   helpers::enrich_tweet_groups(&state, &mut groups).await;
   let content = html! {
       div class="multi-user-timeline" {
           h2 { "Combined timeline: " (usernames.iter().map(|name| format!("@{name}")).collect::<Vec<_>>().join(", ")) }
           (timeline::render_timeline_with_prefs(&groups, &state.config, next_cursor.as_deref(), Some(&base_url), &prefs, newer))
       }
   };

   let markup = layout::PageLayout::new(&state.config, &title, content)
      .prefs(&prefs)
      .render();
   Ok(Html(markup.into_string()).into_response())
}
