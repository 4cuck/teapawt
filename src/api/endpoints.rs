/// Twitter/X API constants and endpoints.
use data_encoding::BASE64;
use serde_json::json;

pub const CONSUMER_KEY: &str = "3nVuSoBZnx6U4vzUxf5w";
pub const CONSUMER_SECRET: &str = "Bcs59EFbbsdF6Sl9Ng71smgStWEGwXXKSjYvPVt7qys";
/// Bearer token that requires x-client-transaction-id (used for cookie sessions
/// with TID).
pub const BEARER_TOKEN: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%\
    3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

/// Fallback bearer token that doesn't require x-client-transaction-id.
pub const BEARER_TOKEN_NO_TID: &str = "Bearer AAAAAAAAAAAAAAAAAAAAAFXzAwAAAAAAMHCxpeSDG1gLNLghVe8d74hl6k4%\
    3DRUMF4xAQLsbeBhTSRrCiQpJtxoGWeyHrDb5te2jpGskWDFW82F";

// GraphQL endpoints
pub const GRAPH_USER: &str = "-oaLodhGbbnzJBACb1kk2Q/UserByScreenName";
pub const GRAPH_USER_BY_ID: &str = "VN33vKXrPT7p35DgNR27aw/UserResultByIdQuery";
pub const GRAPH_USER_TWEETS: &str = "N9_71NodX1yntoC5pa4IFw/UserTweets";
pub const GRAPH_USER_MEDIA: &str = "36oKqyQ7E_9CmtONGjJRsA/UserMedia";
pub const GRAPH_USER_MEDIA_V2: &str = "bp0e_WdXqgNBIwlLukzyYA/MediaTimelineV2";
pub const GRAPH_TWEET_DETAIL: &str = "flqCy6kvOMolEquuRpOaHQ/TweetDetail";
/// Web `SearchTimeline` query, from a logged-in Chrome 152 capture of Latest,
/// Top, People, and Media.
pub const GRAPH_SEARCH_TIMELINE: &str = "ph2fARFabkwfxqmSKQ1OPw/SearchTimeline";
/// Web `ExplorePage` query, from a logged-in Chrome capture of `/explore`.
pub const GRAPH_EXPLORE_PAGE: &str = "BMib5Px-wibex8mYlOUhmQ/ExplorePage";
/// Web `GenericTimelineById` query. Each Explore tab is one of these timelines.
pub const GRAPH_GENERIC_TIMELINE: &str = "cW93hZ3nlNYbYcUROF5r5A/GenericTimelineById";
/// Score, teams, and play ids for one NFL game. Chrome sends this from the
/// game Highlights page.
pub const GRAPH_NFL_GAME: &str = "CN2G3v-dUfNQe3NNv-Lxqw/NFLGameHydrate";
/// Posts attached to a game. The Highlights page loads this timeline.
pub const GRAPH_SPORTS_GAME_TOP: &str = "kYTK7T6VUvMvsIUab_n_cA/SportsGameTopTimeline";
/// Hydrates the clip ids the game page already knows.
pub const GRAPH_TWEETS_BY_IDS: &str = "aiX7QIJ4jp1QR8wT32vkHQ/TweetResultsByRestIds";
/// Season schedule. Used to list games on the Sports tab.
pub const GRAPH_NFL_SCHEDULE: &str = "I0s-QbdS2Bq_PRCSTCoz8g/NFLSchedule";
/// Web `AiTrendByRestId`. `/i/trending/{id}` loads the story writeup and posts.
pub const GRAPH_AI_TREND: &str = "70JIli1rci1EkuMbkjGBiw/AiTrendByRestId";
/// Lists search uses the same query id with `product=Lists`.
pub const GRAPH_SEARCH_LISTS: &str = GRAPH_SEARCH_TIMELINE;
pub const GRAPH_LIST_BY_ID: &str = "T-YE5ZY0cd0UVtWu8AP04Q/ListByRestId";
pub const GRAPH_LIST_BY_SLUG: &str = "K6wihoTiTrzNzSF8y1aeKQ/ListBySlug";
pub const GRAPH_LIST_TWEETS: &str = "wD-euF_1WoOc5VygdYIhYA/ListLatestTweetsTimeline";
pub const GRAPH_LIST_MEMBERS: &str = "BQp2IEYkgxuSxqbTAr1e1g/ListMembers";
pub const GRAPH_USER_TWEETS_AND_REPLIES: &str = "kkaJ0Mf34PZVarrxzLihjg/UserTweetsAndReplies";
/// Web query for a profile's Reposts tab, from a logged-in Chrome capture.
pub const GRAPH_USER_REPOSTS: &str = "xqXxsWU5PFYzr90dfab6rg/UserRepostsTimeline";
pub const GRAPH_USER_TWEETS_AND_REPLIES_V2: &str =
   "BDX77Xzqypdt11-mDfgdpQ/UserWithProfileTweetsAndRepliesQueryV2";
pub const GRAPH_TWEET_EDIT_HISTORY: &str = "upS9teTSG45aljmP9oTuXA/TweetEditHistory";
pub const GRAPH_RETWEETERS: &str = "tj-dlOvzRKjw69iy4z3LzQ/Retweeters";
pub const GRAPH_ABOUT_ACCOUNT: &str = "XRqGa7EeokUU5kppkh13EA/AboutAccountQuery";
pub const GRAPH_AUDIO_SPACE: &str = "xpwpkJD3FetGBaSq7zH4Lw/AudioSpaceById";
pub const GRAPH_COMMUNITY: &str = "-ElI1vg3dYbttVMhBhGdLw/CommunityQuery";
pub const GRAPH_COMMUNITY_TWEETS: &str = "EwftYyqQemkckQ0wzGM6uw/CommunityTweetsTimeline";
pub const GRAPH_COMMUNITY_MEDIA: &str = "ESJtwnI_apuGesbJncpc0Q/CommunityMediaTimeline";
pub const BROADCAST_SHOW_PATH: &str = "/i/api/1.1/broadcasts/show.json";
pub const LIVE_VIDEO_STREAM_PATH: &str = "/i/api/1.1/live_video_stream/status/";

/// Strato translation keeps its own budget, so it must not be accounted
/// against `TweetDetail`.
pub const STRATO_TRANSLATE: &str = "strato/translateTweet";

// Base URLs
pub const GRAPHQL_URL: &str = "https://x.com/i/api/graphql";
pub const API_URL: &str = "https://api.x.com/graphql";
pub const STRATO_URL: &str = "https://x.com/i/api/1.1/strato/column/None";
pub const ACCOUNT_SETTINGS_URL: &str = "https://api.x.com/1.1/account/settings.json";
pub const UPDATE_PROFILE_URL: &str = "https://api.x.com/1.1/account/update_profile.json";
pub const VERIFY_CREDENTIALS_URL: &str = "https://api.x.com/1.1/account/verify_credentials.json";
pub const ACCOUNT_SETTINGS: &str = "account/settings";
pub const UPDATE_PROFILE: &str = "account/update_profile";
pub const VERIFY_CREDENTIALS: &str = "account/verify_credentials";
pub const SEARCH_SAFETY: &str = "search/searchSafety";
pub const X_POSED_LOOKUP_URL: &str = "https://x-posed-cache.xaitax.workers.dev/lookup";

/// GraphQL features to include in requests.
pub const GQL_FEATURES: &str = r#"{"android_ad_formats_media_component_render_overlay_enabled":false,"android_graphql_skip_api_media_color_palette":false,"android_professional_link_spotlight_display_enabled":false,"blue_business_profile_image_shape_enabled":false,"commerce_android_shop_module_enabled":false,"creator_subscriptions_subscription_count_enabled":false,"creator_subscriptions_tweet_preview_api_enabled":true,"freedom_of_speech_not_reach_fetch_enabled":true,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"hidden_profile_likes_enabled":false,"highlights_tweets_tab_ui_enabled":false,"interactive_text_enabled":false,"longform_notetweets_consumption_enabled":true,"longform_notetweets_inline_media_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_richtext_consumption_enabled":true,"mobile_app_spotlight_module_enabled":false,"responsive_web_edit_tweet_api_enabled":true,"responsive_web_enhance_cards_enabled":false,"responsive_web_graphql_exclude_directive_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"responsive_web_graphql_timeline_navigation_enabled":true,"responsive_web_media_download_video_enabled":false,"responsive_web_text_conversations_enabled":false,"responsive_web_twitter_article_tweet_consumption_enabled":true,"unified_cards_destination_url_params_enabled":false,"responsive_web_twitter_blue_verified_badge_is_enabled":true,"rweb_lists_timeline_redesign_enabled":true,"spaces_2022_h2_clipping":true,"spaces_2022_h2_spaces_communities":true,"standardized_nudges_misinfo":true,"subscriptions_verification_info_enabled":true,"subscriptions_verification_info_reason_enabled":true,"subscriptions_verification_info_verified_since_enabled":true,"super_follow_badge_privacy_enabled":false,"super_follow_exclusive_tweet_notifications_enabled":false,"super_follow_tweet_api_enabled":false,"super_follow_user_api_enabled":false,"tweet_awards_web_tipping_enabled":false,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"tweetypie_unmention_optimization_enabled":false,"unified_cards_ad_metadata_container_dynamic_card_content_query_enabled":false,"verified_phone_label_enabled":false,"vibe_api_enabled":false,"view_counts_everywhere_api_enabled":true,"premium_content_api_read_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"responsive_web_jetfuel_frame":true,"responsive_web_grok_analyze_button_fetch_trends_enabled":false,"responsive_web_grok_image_annotation_enabled":true,"responsive_web_grok_imagine_annotation_enabled":true,"rweb_tipjar_consumption_enabled":true,"profile_label_improvements_pcf_label_in_post_enabled":true,"creator_subscriptions_quote_tweet_preview_enabled":false,"c9s_tweet_anatomy_moderator_badge_enabled":true,"responsive_web_grok_analyze_post_followups_enabled":true,"rweb_video_timestamps_enabled":false,"responsive_web_grok_share_attachment_enabled":true,"articles_preview_enabled":true,"immersive_video_status_linkable_timestamps":false,"articles_api_enabled":false,"responsive_web_grok_analysis_button_from_backend":true,"rweb_video_screen_enabled":false,"payments_enabled":false,"responsive_web_profile_redirect_enabled":false,"responsive_web_grok_show_grok_translated_post":false,"responsive_web_grok_community_note_auto_translation_is_enabled":false,"profile_label_improvements_pcf_label_in_profile_enabled":false,"grok_android_analyze_trend_fetch_enabled":false,"grok_translations_community_note_auto_translation_is_enabled":false,"grok_translations_post_auto_translation_is_enabled":false,"grok_translations_community_note_translation_is_enabled":false,"grok_translations_timeline_user_bio_auto_translation_is_enabled":false,"subscriptions_feature_can_gift_premium":false,"responsive_web_twitter_article_notes_tab_enabled":false,"subscriptions_verification_info_is_identity_verified_enabled":false,"hidden_profile_subscriptions_enabled":false,"content_disclosure_indicator_enabled":true,"content_disclosure_ai_generated_indicator_enabled":true}"#;

pub const USER_FIELD_TOGGLES: &str = r#"{"withPayments":false,"withAuxiliaryUserLabels":true}"#;
pub const USER_TWEETS_FIELD_TOGGLES: &str = r#"{"withArticlePlainText":false}"#;
pub const USER_REPOSTS_FIELD_TOGGLES: &str =
   r#"{"withPayments":false,"withArticlePlainText":false}"#;
/// Features sent with `UserRepostsTimeline` in the same capture. The query
/// id is paired with this set; the older shared blob is a different generation.
pub const USER_REPOSTS_FEATURES: &str = r#"{"rweb_video_screen_enabled":false,"rweb_cashtags_enabled":true,"profile_label_improvements_pcf_label_in_post_enabled":true,"responsive_web_profile_redirect_enabled":true,"rweb_tipjar_consumption_enabled":false,"verified_phone_label_enabled":false,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_timeline_navigation_enabled":true,"premium_content_api_read_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"responsive_web_grok_analyze_button_fetch_trends_enabled":false,"responsive_web_grok_analyze_post_followups_enabled":true,"rweb_cashtags_composer_attachment_enabled":true,"responsive_web_jetfuel_frame":true,"rweb_sports_post_context_enabled":true,"responsive_web_grok_share_attachment_enabled":true,"responsive_web_grok_annotations_enabled":true,"articles_preview_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"rweb_conversational_replies_downvote_enabled":false,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"view_counts_everywhere_api_enabled":true,"longform_notetweets_consumption_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"content_disclosure_indicator_enabled":true,"content_disclosure_ai_generated_indicator_enabled":true,"responsive_web_grok_show_grok_translated_post":true,"responsive_web_grok_analysis_button_from_backend":true,"post_ctas_fetch_enabled":false,"freedom_of_speech_not_reach_fetch_enabled":true,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_inline_media_enabled":false,"responsive_web_nested_quote_preview_enabled":true,"responsive_web_grok_image_annotation_enabled":true,"responsive_web_grok_imagine_annotation_enabled":true,"responsive_web_grok_community_note_auto_translation_is_enabled":true,"responsive_web_enhance_cards_enabled":false}"#;
/// Same feature set the web client sent with `SearchTimeline` (tweets, people,
/// media, and lists) in the Chrome 152 capture.
pub const SEARCH_FEATURES: &str = USER_REPOSTS_FEATURES;
/// Features sent with `ExplorePage` and `GenericTimelineById` in the Chrome
/// capture of Explore. Sports context is off; otherwise this matches search.
pub const EXPLORE_FEATURES: &str = r#"{"articles_preview_enabled":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"communities_web_enable_tweet_community_results_fetch":true,"content_disclosure_ai_generated_indicator_enabled":true,"content_disclosure_indicator_enabled":true,"creator_subscriptions_tweet_preview_api_enabled":true,"freedom_of_speech_not_reach_fetch_enabled":true,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"longform_notetweets_consumption_enabled":true,"longform_notetweets_inline_media_enabled":false,"longform_notetweets_rich_text_read_enabled":true,"post_ctas_fetch_enabled":false,"premium_content_api_read_enabled":false,"profile_label_improvements_pcf_label_in_post_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"responsive_web_enhance_cards_enabled":false,"responsive_web_graphql_timeline_navigation_enabled":true,"responsive_web_grok_analysis_button_from_backend":true,"responsive_web_grok_analyze_button_fetch_trends_enabled":false,"responsive_web_grok_analyze_post_followups_enabled":true,"responsive_web_grok_annotations_enabled":true,"responsive_web_grok_community_note_auto_translation_is_enabled":true,"responsive_web_grok_image_annotation_enabled":true,"responsive_web_grok_imagine_annotation_enabled":true,"responsive_web_grok_share_attachment_enabled":true,"responsive_web_grok_show_grok_translated_post":true,"responsive_web_jetfuel_frame":true,"responsive_web_nested_quote_preview_enabled":true,"responsive_web_profile_redirect_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"rweb_cashtags_composer_attachment_enabled":true,"rweb_cashtags_enabled":true,"rweb_conversational_replies_downvote_enabled":false,"rweb_sports_post_context_enabled":false,"rweb_tipjar_consumption_enabled":false,"rweb_video_screen_enabled":false,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"verified_phone_label_enabled":false,"view_counts_everywhere_api_enabled":true}"#;
/// Features sent with `TweetResultsByRestIds` on the game Highlights page.
pub const TWEET_RESULTS_FEATURES: &str = r#"{"creator_subscriptions_tweet_preview_api_enabled":true,"premium_content_api_read_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"responsive_web_grok_analyze_button_fetch_trends_enabled":false,"responsive_web_grok_analyze_post_followups_enabled":true,"rweb_cashtags_composer_attachment_enabled":true,"responsive_web_jetfuel_frame":true,"rweb_sports_post_context_enabled":false,"responsive_web_grok_share_attachment_enabled":true,"responsive_web_grok_annotations_enabled":true,"articles_preview_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"rweb_conversational_replies_downvote_enabled":false,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"view_counts_everywhere_api_enabled":true,"longform_notetweets_consumption_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"content_disclosure_indicator_enabled":true,"content_disclosure_ai_generated_indicator_enabled":true,"responsive_web_grok_show_grok_translated_post":true,"responsive_web_grok_analysis_button_from_backend":true,"post_ctas_fetch_enabled":false,"freedom_of_speech_not_reach_fetch_enabled":true,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"rweb_cashtags_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_inline_media_enabled":false,"profile_label_improvements_pcf_label_in_post_enabled":true,"responsive_web_profile_redirect_enabled":true,"rweb_tipjar_consumption_enabled":false,"verified_phone_label_enabled":false,"responsive_web_nested_quote_preview_enabled":true,"responsive_web_grok_image_annotation_enabled":true,"responsive_web_grok_imagine_annotation_enabled":true,"responsive_web_grok_community_note_auto_translation_is_enabled":true,"responsive_web_graphql_timeline_navigation_enabled":true}"#;
pub const TWEET_RESULTS_TOGGLES: &str = r#"{"withArticleRichContentState":true,"withArticlePlainText":false,"withArticleSummaryText":true,"withArticleVoiceOver":true,"withPayments":false}"#;
/// Same feature set the web client sent with the Lists search query.
pub const LIST_SEARCH_FEATURES: &str = SEARCH_FEATURES;
/// Features sent with `ListByRestId` when a list page opens.
pub const LIST_BY_ID_FEATURES: &str = r#"{"profile_label_improvements_pcf_label_in_post_enabled":true,"responsive_web_profile_redirect_enabled":true,"rweb_tipjar_consumption_enabled":false,"verified_phone_label_enabled":false,"responsive_web_graphql_timeline_navigation_enabled":true}"#;
pub const TWEET_DETAIL_FIELD_TOGGLES: &str = r#"{"withArticleRichContentState":true,"withArticlePlainText":false,"withGrokAnalyze":false,"withDisallowedReplyControls":false}"#;

// ── Helper ──────────────────────────────────────────────────────────────

/// Serialize a JSON value to string, stripping null fields (for optional
/// cursor).
fn vars(mut value: serde_json::Value) -> String {
   if let Some(obj) = value.as_object_mut() {
      obj.retain(|_, val| !val.is_null());
   }
   value.to_string()
}

// ── Builder functions ───────────────────────────────────────────────────

pub fn tweet_detail_vars(focal_tweet_id: &str, cursor: Option<&str>, ranking_mode: &str) -> String {
   vars(json!({
      "focalTweetId": focal_tweet_id, "cursor": cursor,
      "referrer": "profile", "withRuxInjections": false, "rankingMode": ranking_mode,
      "includePromotedContent": false, "withCommunity": true,
      "withQuickPromoteEligibilityTweetFields": true,
      "withBirdwatchNotes": true, "withVoice": true,
   }))
}

pub fn user_tweets_vars(user_id: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "userId": user_id, "cursor": cursor, "count": 20,
      "includePromotedContent": false,
      "withQuickPromoteEligibilityTweetFields": true, "withVoice": true,
   }))
}

pub fn user_reposts_vars(user_id: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "userId": user_id, "cursor": cursor, "count": 20,
      "includePromotedContent": true, "withVoice": true,
   }))
}

pub fn user_media_vars(user_id: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "userId": user_id, "cursor": cursor, "count": 20,
      "includePromotedContent": false, "withClientEventToken": false,
      "withBirdwatchNotes": false, "withVoice": true,
   }))
}

pub fn user_media_v2_vars(user_id: &str, cursor: Option<&str>) -> String {
   vars(json!({ "rest_id": user_id, "cursor": cursor, "count": 20 }))
}

pub fn user_tweets_and_replies_v2_vars(user_id: &str, cursor: Option<&str>) -> String {
   vars(json!({ "rest_id": user_id, "cursor": cursor, "count": 20 }))
}

/// As Chrome sent them with `UserByScreenName` on a profile page.
pub fn user_by_screen_name_vars(screen_name: &str) -> String {
   vars(json!({ "screen_name": screen_name, "withGrokTranslatedBio": true }))
}

pub fn about_account_vars(screen_name: &str) -> String {
   vars(json!({ "screenName": screen_name }))
}

pub fn user_by_id_vars(rest_id: &str) -> String {
   json!({ "rest_id": rest_id }).to_string()
}

pub fn list_members_vars(list_id: &str, cursor: Option<&str>) -> String {
   vars(json!({ "listId": list_id, "cursor": cursor, "count": 20 }))
}

pub fn user_tweets_and_replies_vars(user_id: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "userId": user_id, "cursor": cursor, "count": 20,
      "includePromotedContent": false, "withCommunity": true, "withVoice": true,
   }))
}

pub fn list_by_slug_vars(screen_name: &str, list_slug: &str) -> String {
   json!({ "screenName": screen_name, "listSlug": list_slug }).to_string()
}

pub fn tweet_edit_history_vars(tweet_id: &str) -> String {
   json!({ "tweetId": tweet_id, "withQuickPromoteEligibilityTweetFields": true }).to_string()
}

pub fn retweeters_vars(tweet_id: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "tweetId": tweet_id, "cursor": cursor, "count": 20,
      "includePromotedContent": false,
   }))
}

pub fn audio_space_vars(space_id: &str) -> String {
   json!({
      "id": space_id,
      "isMetatagsQuery": false,
      "withReplays": true,
      "withListeners": true,
   })
   .to_string()
}

pub fn broadcast_show_url(broadcast_id: &str) -> String {
   format!("https://x.com{BROADCAST_SHOW_PATH}?ids={broadcast_id}&include_events=false")
}

pub fn live_stream_url(media_key: &str) -> String {
   format!("https://x.com{LIVE_VIDEO_STREAM_PATH}{media_key}")
}

pub fn community_vars(community_id: &str) -> String {
   vars(json!({ "communityId": community_id }))
}

pub fn community_tweets_vars(
   community_id: &str,
   cursor: Option<&str>,
   ranking_mode: &str,
) -> String {
   vars(json!({
      "communityId": community_id,
      "cursor": cursor,
      "count": 20,
      "displayLocation": "Community",
      "rankingMode": ranking_mode,
      "withCommunity": true,
   }))
}

pub fn community_media_vars(community_id: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "communityId": community_id,
      "cursor": cursor,
      "count": 20,
      "withCommunity": true,
   }))
}

/// Prefix of every Explore tab id in the Chrome capture:
/// `Timeline:` plus the fixed bytes that precede the tab name.
const EXPLORE_TIMELINE_MARK: &[u8] = b"Timeline:\x0c\x00\xb6\x0b\x00\x01\x00\x00\x00";

/// `ExplorePage` always sends `cursor`, empty on the first page.
pub fn nfl_game_vars(game_id: &str) -> String {
   json!({ "includeRecentGames": false, "rest_id": game_id }).to_string()
}

pub fn sports_game_top_vars(game_id: &str) -> String {
   json!({ "gameId": game_id, "count": 20 }).to_string()
}

pub fn tweets_by_ids_vars(ids: &[String]) -> String {
   json!({
      "tweetIds": ids,
      "includePromotedContent": true,
      "withBirdwatchNotes": true,
      "withVoice": true,
      "withCommunity": true,
   })
   .to_string()
}

/// The schedule window Chrome sent while the Highlights page was open.
pub fn ai_trend_vars(trend_id: &str) -> String {
   json!({
      "trendId": trend_id,
      "includePromotedContent": false,
      "withBirdwatchNotes": false,
      "withVoice": false,
      "withCommunity": false,
   })
   .to_string()
}

pub fn nfl_schedule_vars() -> String {
   json!({
      "start_time": "1785556800000",
      "end_time": "1803877200000",
   })
   .to_string()
}

pub fn explore_page_vars(cursor: Option<&str>) -> String {
   json!({ "cursor": cursor.unwrap_or("") }).to_string()
}

/// Variables for one Explore tab. The cursor is omitted on the first page,
/// matching the capture.
pub fn generic_timeline_vars(timeline_id: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "timelineId": timeline_id,
      "count": 20,
      "cursor": cursor,
      "withQuickPromoteEligibilityTweetFields": true,
   }))
}

/// Base64 timeline id for an Explore tab name (`trending`, `highlights`,
/// `for_you`). The shape is the one Chrome sent for each `/explore/tabs/` page.
pub fn explore_timeline_id(name: &str) -> Option<String> {
   if name.is_empty() || name.len() > 40 || !name.is_ascii() {
      return None;
   }
   let mut raw = Vec::with_capacity(EXPLORE_TIMELINE_MARK.len() + name.len() + 3);
   raw.extend_from_slice(EXPLORE_TIMELINE_MARK);
   let Ok(len) = u8::try_from(name.len()) else {
      return None;
   };
   raw.push(len);
   raw.extend_from_slice(name.as_bytes());
   raw.extend_from_slice(&[0, 0]);
   Some(BASE64.encode(&raw))
}

pub fn explore_timeline_name(id: &str) -> Option<String> {
   let raw = BASE64.decode(id.as_bytes()).ok()?;
   let rest = raw.strip_prefix(EXPLORE_TIMELINE_MARK)?;
   let (len, rest) = rest.split_first()?;
   let len = usize::from(*len);
   if rest.len() != len + 2 || rest.get(len..)? != [0, 0] {
      return None;
   }
   let name = str::from_utf8(rest.get(..len)?).ok()?;
   Some(name.to_owned())
}

pub fn search_vars(raw_query: &str, cursor: Option<&str>, product: &str) -> String {
   vars(json!({
      "rawQuery": raw_query,
      "count": 20,
      "cursor": cursor,
      "querySource": "typed_query",
      "product": product,
      "withGrokTranslatedBio": false,
      "withQuickPromoteEligibilityTweetFields": false,
   }))
}

/// Characters `encodeURIComponent` leaves alone, which is how the web client
/// writes `q=` into the search page URL.
const URI_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
   .remove(b'-')
   .remove(b'_')
   .remove(b'.')
   .remove(b'!')
   .remove(b'~')
   .remove(b'*')
   .remove(b'\'')
   .remove(b'(')
   .remove(b')');

/// The search page a `SearchTimeline` call is made from, which Chrome sends
/// as its referer. Other operations keep the client's default referer.
pub fn search_referer(endpoint: &str, variables: &str) -> Option<String> {
   if !endpoint.ends_with("/SearchTimeline") {
      return None;
   }
   let variables: serde_json::Value = serde_json::from_str(variables).ok()?;
   let query = variables.get("rawQuery")?.as_str()?;
   let tab = match variables.get("product")?.as_str()? {
      "Latest" => "&f=live",
      "People" => "&f=user",
      "Media" => "&f=media",
      "Lists" => "&f=list",
      _ => "",
   };
   Some(format!(
      "https://x.com/search?q={}&src=typed_query{tab}",
      percent_encoding::utf8_percent_encode(query, URI_COMPONENT)
   ))
}

/// The Explore page a tab request is made from. Chrome sends `/explore` for
/// the home of Explore and `/explore/tabs/{name}` for every other tab.
pub fn explore_referer(endpoint: &str, variables: &str) -> Option<String> {
   if endpoint.ends_with("/ExplorePage") {
      return Some("https://x.com/explore".to_owned());
   }
   if !endpoint.ends_with("/GenericTimelineById") {
      return None;
   }
   let variables: serde_json::Value = serde_json::from_str(variables).ok()?;
   let id = variables.get("timelineId")?.as_str()?;
   let name = explore_timeline_name(id)?;
   if name == "for_you" {
      Some("https://x.com/explore".to_owned())
   } else {
      Some(format!("https://x.com/explore/tabs/{name}"))
   }
}

/// Referer for a trending story. Chrome calls `AiTrendByRestId` from
/// `/i/trending/{id}`.
pub fn trend_referer(endpoint: &str, variables: &str) -> Option<String> {
   if !endpoint.ends_with("/AiTrendByRestId") {
      return None;
   }
   let variables: serde_json::Value = serde_json::from_str(variables).ok()?;
   let id = variables.get("trendId")?.as_str()?;
   Some(format!("https://x.com/i/trending/{id}"))
}

/// Referer for the NFL game APIs, which Chrome calls from the Highlights page.
pub fn game_referer(endpoint: &str, variables: &str) -> Option<String> {
   let game_call = endpoint.ends_with("/NFLGameHydrate")
      || endpoint.ends_with("/SportsGameTopTimeline")
      || endpoint.ends_with("/TweetResultsByRestIds")
      || endpoint.ends_with("/NFLSchedule");
   if !game_call {
      return None;
   }
   let variables: serde_json::Value = serde_json::from_str(variables).ok()?;
   let id = variables
      .get("rest_id")
      .or_else(|| variables.get("gameId"))
      .and_then(serde_json::Value::as_str)
      .unwrap_or("2055084502741811200");
   Some(format!("https://x.com/i/nfl/game/{id}?page=highlights"))
}

/// Variables for the web client's Lists tab. `querySource` is `typed_query`
/// on this query id, and the cursor is omitted on the first page.
pub fn search_list_vars(raw_query: &str, cursor: Option<&str>) -> String {
   vars(json!({
      "rawQuery": raw_query, "cursor": cursor, "count": 20,
      "querySource": "typed_query", "product": "Lists",
      "withGrokTranslatedBio": false,
      "withQuickPromoteEligibilityTweetFields": false,
   }))
}

pub fn list_by_id_vars(list_id: &str) -> String {
   json!({ "listId": list_id }).to_string()
}

pub fn list_timeline_vars(list_id: &str, cursor: Option<&str>) -> String {
   vars(json!({ "listId": list_id, "cursor": cursor, "count": 20 }))
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn search_vars_match_the_chrome_capture() {
      let first: serde_json::Value =
         serde_json::from_str(&search_vars("test", None, "Latest")).unwrap();
      assert_eq!(first["rawQuery"], "test");
      assert_eq!(first["count"], 20);
      assert_eq!(first["querySource"], "typed_query");
      assert_eq!(first["product"], "Latest");
      assert_eq!(first["withGrokTranslatedBio"], false);
      assert_eq!(first["withQuickPromoteEligibilityTweetFields"], false);
      assert!(first.get("cursor").is_none());
      assert!(first.get("withDownvotePerspective").is_none());

      let page: serde_json::Value =
         serde_json::from_str(&search_vars("test", Some("CURSOR"), "Media")).unwrap();
      assert_eq!(page["count"], 20);
      assert_eq!(page["cursor"], "CURSOR");
      assert_eq!(page["product"], "Media");
   }

   #[test]
   fn search_referer_is_the_search_page() {
      let latest = search_vars("test", None, "Latest");
      assert_eq!(
         search_referer("ph2fARFabkwfxqmSKQ1OPw/SearchTimeline", &latest).as_deref(),
         Some("https://x.com/search?q=test&src=typed_query&f=live")
      );
      let top = search_vars("cats and dogs", None, "Top");
      assert_eq!(
         search_referer("ph2fARFabkwfxqmSKQ1OPw/SearchTimeline", &top).as_deref(),
         Some("https://x.com/search?q=cats%20and%20dogs&src=typed_query")
      );
      assert!(search_referer("x/UserTweets", "{}").is_none());
   }

   #[test]
   fn explore_timeline_ids_match_the_chrome_capture() {
      assert_eq!(
         explore_timeline_id("trending").as_deref(),
         Some("VGltZWxpbmU6DAC2CwABAAAACHRyZW5kaW5nAAA=")
      );
      assert_eq!(
         explore_timeline_id("news").as_deref(),
         Some("VGltZWxpbmU6DAC2CwABAAAABG5ld3MAAA==")
      );
      assert_eq!(
         explore_timeline_id("sports").as_deref(),
         Some("VGltZWxpbmU6DAC2CwABAAAABnNwb3J0cwAA")
      );
      assert_eq!(
         explore_timeline_id("entertainment").as_deref(),
         Some("VGltZWxpbmU6DAC2CwABAAAADWVudGVydGFpbm1lbnQAAA==")
      );
      assert_eq!(
         explore_timeline_id("for_you").as_deref(),
         Some("VGltZWxpbmU6DAC2CwABAAAAB2Zvcl95b3UAAA==")
      );
      assert_eq!(
         explore_timeline_id("highlights").as_deref(),
         Some("VGltZWxpbmU6DAC2CwABAAAACmhpZ2hsaWdodHMAAA==")
      );
      assert_eq!(
         explore_timeline_name("VGltZWxpbmU6DAC2CwABAAAACHRyZW5kaW5nAAA=").as_deref(),
         Some("trending")
      );
      assert!(explore_timeline_id("").is_none());
   }

   #[test]
   fn explore_referer_is_the_explore_page() {
      assert_eq!(
         explore_referer("BMib5Px-wibex8mYlOUhmQ/ExplorePage", r#"{"cursor":""}"#).as_deref(),
         Some("https://x.com/explore")
      );
      let trending = generic_timeline_vars(
         &explore_timeline_id("highlights").expect("highlights id"),
         None,
      );
      assert_eq!(
         explore_referer("cW93hZ3nlNYbYcUROF5r5A/GenericTimelineById", &trending).as_deref(),
         Some("https://x.com/explore/tabs/highlights")
      );
      let first: serde_json::Value = serde_json::from_str(&explore_page_vars(None)).expect("vars");
      assert_eq!(first["cursor"], "");
      let page: serde_json::Value =
         serde_json::from_str(&generic_timeline_vars("ID", Some("CURSOR"))).expect("vars");
      assert_eq!(page["cursor"], "CURSOR");
      assert_eq!(page["count"], 20);
      assert!(
         serde_json::from_str::<serde_json::Value>(&generic_timeline_vars("ID", None))
            .expect("vars")
            .get("cursor")
            .is_none()
      );
   }
}

/// Build the Strato translate tweet URL.
pub fn translate_url(tweet_id: &str) -> String {
   format!(
      "{STRATO_URL}/tweetId={tweet_id},destinationLanguage=None,translationSource=Some(Google),\
       feature=None,timeout=None,onlyCached=None/translation/service/translateTweet"
   )
}
