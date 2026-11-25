//! Documentation for the search-pass JSON API.

use maud::{
   Markup,
   html,
};

/// The docs page. `origin` is this instance as the reader reached it, and
/// `open` means this request may call the endpoints.
pub fn render(origin: &str, open: bool) -> Markup {
   html! {
       div class="overlay-panel api-doc" {
           h1 { "API" }
           p {
               "Compact JSON for search, profiles, and posts. "
               "A page of results is the posts themselves, without the HTML chrome, so a client downloads much less than "
               a href="/search" { "/search" }
               ". The machine-readable description is "
               a href="/api/openapi.json" { "OpenAPI" }
               "."
           }
           @if open {
               p class="pass-notice is-good" {
                   "This browser's search pass can call the endpoints below."
               }
           } @else {
               p class="pass-notice is-info" {
                   "A search pass is required. "
                   a href="/search-pass" { "Get one" }
                   ", or "
                   a href="/search-pass/login" { "sign in" }
                   ". A whitelisted address can call with no cookie."
               }
           }

           h2 { "Authentication" }
           p {
               "Create a key on "
               a href="/search-pass" { "the search pass page" }
               ". It is 256 random bits, shown once. Send it on every call:"
           }
           pre { code { "curl -sS --compressed \\\n  -H 'Authorization: Bearer tpk_…' \\\n  '" (origin) "/api/search?q=rust&f=latest&f-images=on'" } }
           p {
               code { "X-Api-Key: tpk_…" }
               " is the same key. A signed-in browser can use the "
               code { "paid_search_pass" }
               " cookie instead, and a whitelisted address needs neither. "
               "The same key skips the checkbox on "
               a href="/search" { "/search" }
               " and on search RSS. "
               "The five-minute search checkbox does not unlock this."
           }
           p {
               "Responses are "
               code { "application/json" }
               " and "
               code { "Cache-Control: private, no-store" }
               ". Ask for gzip with "
               code { "Accept-Encoding" }
               " and the body is compressed. Empty fields are left out. Post ids are strings, so they stay exact in JavaScript. "
               "Text is as X sends it: links are "
               code { "t.co" }
               " links, and most posts arrive with "
               code { "&amp;" }
               ", "
               code { "&lt;" }
               " and "
               code { "&gt;" }
               " escaped."
           }

           h2 { "Search" }
           p {
               code { "GET /api/search?q=QUERY" }
               " — "
               code { "q" }
               " is required (512 characters at most) unless "
               code { "from" }
               " is set. "
               code { "f" }
               " is "
               code { "latest" }
               " (the default; "
               code { "tweets" }
               " is the same), "
               code { "top" }
               ", "
               code { "media" }
               ", "
               code { "users" }
               ", or "
               code { "lists" }
               ". "
               code { "f=media" }
               " keeps posts that have an image, video, GIF, poll image, or link-card image."
           }
           p {
               "The search page's filters work here too. "
               code { "from" }
               ", "
               code { "since" }
               " ("
               code { "YYYY-MM-DD" }
               "), "
               code { "until" }
               ", and "
               code { "min_faves" }
               ". "
               "Include toggles, value "
               code { "on" }
               " (also "
               code { "true" }
               ", "
               code { "1" }
               ", or "
               code { "yes" }
               "): "
               code { "f-media" }
               ", "
               code { "f-images" }
               ", "
               code { "f-videos" }
               ", "
               code { "f-native_video" }
               ", "
               code { "f-links" }
               ", "
               code { "f-news" }
               ", "
               code { "f-quote" }
               ", "
               code { "f-replies" }
               ", "
               code { "f-spaces" }
               ", "
               code { "f-verified" }
               ", "
               code { "f-nativeretweets" }
               ". "
               "Exclude toggles use "
               code { "e-" }
               " and the same names ("
               code { "e-replies" }
               ", "
               code { "e-nativeretweets" }
               ", "
               code { "e-media" }
               ", and the rest). "
               "A comma list does the same job: "
               code { "filters=images,links" }
               " and "
               code { "exclude=replies" }
               ". "
               "Operators inside "
               code { "q" }
               " work as well, for example "
               code { "from:name since:2024-01-01 filter:images" }
               ". "
               "Checking both include and exclude for one name keeps the exclude."
           }
           p {
               "Pass "
               code { "meta.cursor" }
               " back as "
               code { "cursor" }
               " for the next page. It is left out when there is no further page. "
               "A page of posts can be empty and still have a cursor: every post on it was filtered out, and there are more behind it. "
               "Stop when the cursor is gone, or after two empty pages in a row. "
               "Searches run on the search-pass accounts."
           }
           pre { code {
"{
  \"data\": [{
    \"id\": \"20\",
    \"text\": \"just setting up my twttr\",
    \"created_at\": \"2006-03-21T20:50:14Z\",
    \"lang\": \"en\",
    \"author\": {\"id\": \"12\", \"username\": \"jack\", \"name\": \"jack\"},
    \"metrics\": {\"reposts\": 1, \"likes\": 2}
  }],
  \"meta\": {\"result_count\": 1, \"cursor\": \"…\"}
}"
           } }

           h2 { "Profiles and posts" }
           p { "Each path is the compact form of the X call named beside it. The same " code { "cursor" } ", " code { "tweet.fields" } ", and " code { "user.fields" } " parameters apply." }
           ul {
               li { code { "GET /api/search" } " — SearchTimeline." }
               li { code { "GET /api/lists/{id}/tweets" } " — ListLatestTweetsTimeline." }
               li { code { "GET /api/communities/{id}/tweets?ranking=latest" } " — CommunityTweetsTimeline. " code { "ranking=top" } " is the relevance order." }
               li { code { "GET /api/users/{username}" } " — UserByScreenName." }
               li { code { "GET /api/users/id/{id}" } " — UserByRestId." }
               li { code { "GET /api/users/{username}/tweets" } " — that account's posts." }
               li { code { "GET /api/users/{username}/with_replies" } " — UserWithProfileTweetsAndRepliesQueryV2." }
               li { code { "GET /api/users/{username}/about" } " — AboutAccountQuery." }
               li { code { "GET /api/users/{username}/following" } " — Following." }
               li { code { "GET /api/users/{username}/following/ids?count=5000" } " — 1.1/friends/ids. Ids only, up to 5000." }
               li { code { "GET /api/users/{username}/friends?count=200" } " — 1.1/friends/list." }
               li { code { "GET /api/users/{username}/followers?count=200" } " — 1.1/followers/list." }
               li { code { "GET /api/tweets/{id}" } " — TweetDetail, as " code { "{ \"data\": { … } }" } "." }
               li { code { "GET /api/tweets?ids=1,2,3" } " — TweetResultsByRestIds, up to 100 ids." }
               li { code { "GET /api/tweets/{id}/retweeters" } " — Retweeters." }
           }
           p {
               "A repost is the original post plus "
               code { "reposted_by" }
               " and "
               code { "repost_id" }
               ". A quote is nested once, without its own quote or video variant list. "
               "Missing metric counts are zero. "
               code { "protected" }
               " and "
               code { "suspended" }
               " are present only when true. "
               "The posts and the follow lists of a suspended or protected account answer 403. "
               "Media URLs point at the CDN. "
               code { "entities.start" }
               " and "
               code { "entities.end" }
               " count characters (Unicode code points, not bytes) into "
               code { "text" }
               "."
           }

           h2 { "Fields" }
           p {
               "Omit "
               code { "tweet.fields" }
               " for the small default: "
               code { "created_at" }
               ", "
               code { "lang" }
               ", "
               code { "author" }
               ", "
               code { "metrics" }
               ", "
               code { "reply" }
               ", "
               code { "media" }
               ", "
               code { "quote" }
               ", "
               code { "repost" }
               ", "
               code { "sensitive" }
               ". "
               code { "id" }
               " and "
               code { "text" }
               " are always included. "
               "Naming fields replaces that default, as on X's recent search. "
               code { "tweet.fields=all" }
               " asks for every field."
           }
           p { "Extra tweet fields:" }
           table {
               thead { tr { th { "Name" } th { "Adds" } } }
               tbody {
                   tr { td { code { "entities" } } td { "Mentions, hashtags, symbols, and links, each with where it sits in the text. A link has its full " code { "url" } "." } }
                   tr { td { code { "card" } } td { "Link preview: title, description, image." } }
                   tr { td { code { "poll" } } td { "Choices, votes, and end time." } }
                   tr { td { code { "source" } } td { "Client the post was sent from." } }
                   tr { td { code { "location" } } td { "Place name on the post." } }
                   tr { td { code { "note" } } td { "Community note text." } }
                   tr { td { code { "history" } } td { "Edit-history ids, when there is more than one." } }
                   tr { td { code { "variants" } } td { "Every video format. Implies " code { "media" } "." } }
                   tr { td { code { "labels" } } td { "Paid promotion and AI-generated flags." } }
                   tr { td { code { "reply" } } td { code { "reply_to" } ", " code { "in_reply_to_status_id" } ", " code { "conversation_id" } "." } }
               }
           }
           p {
               "On a post, "
               code { "author" }
               " is "
               code { "id" }
               ", "
               code { "username" }
               ", and "
               code { "name" }
               ". "
               code { "user.fields" }
               " adds "
               code { "verified" }
               ", "
               code { "avatar" }
               ", "
               code { "banner" }
               ", "
               code { "bio" }
               ", "
               code { "location" }
               ", "
               code { "website" }
               ", "
               code { "followers" }
               ", "
               code { "following" }
               ", "
               code { "posts" }
               ", "
               code { "likes" }
               ", "
               code { "media" }
               ", "
               code { "joined" }
               ", "
               code { "pinned" }
               ", "
               code { "protected" }
               ", "
               code { "suspended" }
               ". "
               code { "user.fields=all" }
               " selects those. "
               "A list of accounts (user search, following, followers, friends, retweeters) already includes verification, the avatar, the bio, counts, location, and website. "
               code { "GET /api/users/{username}" }
               " returns the full profile."
           }
           pre { code { (origin) "/api/search?q=rust&tweet.fields=created_at,author,metrics&user.fields=verified,followers" } }

           h2 { "Errors" }
           p { "Every failure has the same shape, and it is JSON on every path listed above." }
           pre { code { "{ \"errors\": [{ \"code\": \"pass_required\", \"message\": \"A search pass is required.\" }] }" } }
           table {
               thead { tr { th { "Code" } th { "Status" } th { "When" } } }
               tbody {
                   tr { td { code { "pass_required" } } td { "401" } td { "No paid pass on this request, or an API key that was not accepted: mistyped, revoked, or on a pass that has run out." } }
                   tr { td { code { "invalid" } } td { "400" } td { "A missing or malformed parameter, an unknown field, or a bad username or id." } }
                   tr { td { code { "blocked" } } td { "400" } td { "X will not run that search." } }
                   tr { td { code { "not_found" } } td { "404" } td { "No such account or post." } }
                   tr { td { code { "suspended" } } td { "403" } td { "The account is suspended." } }
                   tr { td { code { "protected" } } td { "403" } td { "The account's posts are private." } }
                   tr { td { code { "rate_limited" } } td { "429" } td { "The accounts are out of calls, or this address is calling faster than its budget refills. " code { "Retry-After" } " says how many seconds to wait." } }
                   tr { td { code { "unavailable" } } td { "503" } td { "X returned nothing for this call. Try it again." } }
                   tr { td { code { "error" } } td { "500" } td { "The call failed on this side." } }
               }
           }
       }
   }
}
