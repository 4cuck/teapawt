//! OpenAPI description of the search-pass JSON API.

use serde_json::{
   Value,
   json,
};

const TOGGLES: &[&str] = &[
   "f-media",
   "f-images",
   "f-videos",
   "f-native_video",
   "f-links",
   "f-news",
   "f-quote",
   "f-replies",
   "f-spaces",
   "f-verified",
   "f-nativeretweets",
   "e-media",
   "e-images",
   "e-videos",
   "e-native_video",
   "e-links",
   "e-news",
   "e-quote",
   "e-replies",
   "e-spaces",
   "e-nativeretweets",
];

/// OpenAPI 3.1 document for `/api`. `origin` is this instance, with no path.
pub fn document(origin: &str) -> Value {
   json!({
      "openapi": "3.1.0",
      "info": {
         "title": "teapawt API",
         "version": env!("CARGO_PKG_VERSION"),
         "description": "Compact JSON for search, profiles, posts, lists, communities, and follow graphs. A search pass is required: send the API key, the paid_search_pass cookie, or call from a whitelisted address. Empty fields are omitted. Post and user ids are strings."
      },
      "servers": [{ "url": origin }],
      "security": [
         { "bearerAuth": [] },
         { "apiKey": [] },
         { "cookieAuth": [] }
      ],
      "paths": {
         "/api/search": {
            "get": operation(
               "searchTimeline",
               "SearchTimeline",
               "Search posts, people, or lists. The same filters as the search page apply.",
               search_parameters(),
               &["PostPage", "AccountPage", "ListPage"],
               "Search results. Posts unless f is users or lists.",
            )
         },
         "/api/lists/{id}/tweets": {
            "get": operation(
               "listLatestTweets",
               "ListLatestTweetsTimeline",
               "Latest posts in a list.",
               with_page(vec![snowflake("id", "List id.")]),
               &["PostPage"],
               "Posts in the list.",
            )
         },
         "/api/communities/{id}/tweets": {
            "get": operation(
               "communityTweets",
               "CommunityTweetsTimeline",
               "Posts in a community. ranking=latest is the default. ranking=top asks for relevance.",
               with_page(vec![
                  snowflake("id", "Community id."),
                  json!({
                     "name": "ranking",
                     "in": "query",
                     "schema": { "type": "string", "enum": ["latest", "top"] },
                     "description": "latest is Recency. top is Relevance."
                  }),
               ]),
               &["PostPage"],
               "Posts in the community.",
            )
         },
         "/api/users/{username}": {
            "get": operation(
               "userByScreenName",
               "UserByScreenName",
               "One profile, without its posts.",
               vec![username()],
               &["AccountEnvelope"],
               "The profile.",
            )
         },
         "/api/users/id/{id}": {
            "get": operation(
               "userByRestId",
               "UserByRestId",
               "One profile looked up by numeric id.",
               vec![snowflake("id", "User id.")],
               &["AccountEnvelope"],
               "The profile.",
            )
         },
         "/api/users/{username}/tweets": {
            "get": operation(
               "userTweets",
               "User tweets",
               "Posts from one account.",
               with_page(vec![username()]),
               &["PostPage"],
               "The account's posts.",
            )
         },
         "/api/users/{username}/with_replies": {
            "get": operation(
               "userWithReplies",
               "UserWithProfileTweetsAndRepliesQueryV2",
               "Posts and replies from one account.",
               with_page(vec![username()]),
               &["PostPage"],
               "The account's posts and replies.",
            )
         },
         "/api/users/{username}/about": {
            "get": operation(
               "aboutAccount",
               "AboutAccountQuery",
               "Where X says the account is based, and the connection source.",
               vec![username()],
               &["AboutEnvelope"],
               "About this account. Fields are omitted when X did not send them.",
            )
         },
         "/api/users/{username}/following": {
            "get": operation(
               "following",
               "Following",
               "Accounts this user follows, as profiles.",
               with_page(vec![username()]),
               &["AccountPage"],
               "Accounts they follow.",
            )
         },
         "/api/users/{username}/following/ids": {
            "get": operation(
               "friendIds",
               "1.1/friends/ids",
               "Ids of accounts this user follows. count defaults to 5000 and cannot exceed 5000.",
               with_count(with_page(vec![username()]), 5000),
               &["IdPage"],
               "Followed account ids.",
            )
         },
         "/api/users/{username}/friends": {
            "get": operation(
               "friendsList",
               "1.1/friends/list",
               "Accounts this user follows. count defaults to 20 and cannot exceed 200.",
               with_count(with_page(vec![username()]), 200),
               &["AccountPage"],
               "Accounts they follow.",
            )
         },
         "/api/users/{username}/followers": {
            "get": operation(
               "followersList",
               "1.1/followers/list",
               "Accounts that follow this user. count defaults to 20 and cannot exceed 200.",
               with_count(with_page(vec![username()]), 200),
               &["AccountPage"],
               "Accounts that follow them.",
            )
         },
         "/api/tweets/{id}": {
            "get": operation(
               "tweetDetail",
               "TweetDetail",
               "One post.",
               with_fields(vec![snowflake("id", "Post id.")]),
               &["PostEnvelope"],
               "The post.",
            )
         },
         "/api/tweets": {
            "get": operation(
               "tweetsByIds",
               "TweetResultsByRestIds",
               "Up to 100 posts by id.",
               with_fields(vec![json!({
                  "name": "ids",
                  "in": "query",
                  "required": true,
                  "schema": { "type": "string" },
                  "description": "Comma-separated post ids.",
                  "example": "20,21"
               })]),
               &["PostPage"],
               "The posts, in the requested order when X returned them.",
            )
         },
         "/api/tweets/{id}/retweeters": {
            "get": operation(
               "retweeters",
               "Retweeters",
               "Accounts that reposted a post.",
               with_page(vec![snowflake("id", "Post id.")]),
               &["AccountPage"],
               "Accounts that reposted it.",
            )
         }
      },
      "components": components(),
   })
}

fn operation(
   id: &str,
   summary: &str,
   description: &str,
   parameters: Vec<Value>,
   schemas: &[&str],
   ok: &str,
) -> Value {
   let schema = if schemas.len() == 1 {
      json!({ "$ref": format!("#/components/schemas/{}", schemas[0]) })
   } else {
      json!({
         "oneOf": schemas.iter().map(|name| json!({ "$ref": format!("#/components/schemas/{name}") })).collect::<Vec<_>>()
      })
   };
   json!({
      "operationId": id,
      "summary": summary,
      "description": description,
      "parameters": parameters,
      "responses": {
         "200": {
            "description": ok,
            "content": { "application/json": { "schema": schema } }
         },
         "400": { "$ref": "#/components/responses/Error" },
         "401": { "$ref": "#/components/responses/Error" },
         "403": { "$ref": "#/components/responses/Error" },
         "404": { "$ref": "#/components/responses/Error" },
         "429": { "$ref": "#/components/responses/Error" },
         "503": { "$ref": "#/components/responses/Error" }
      }
   })
}

fn username() -> Value {
   json!({
      "name": "username",
      "in": "path",
      "required": true,
      "schema": { "type": "string", "pattern": "^@?[A-Za-z0-9_]{1,15}$" },
      "description": "X handle. A leading @ is accepted."
   })
}

fn snowflake(name: &str, description: &str) -> Value {
   json!({
      "name": name,
      "in": "path",
      "required": true,
      "schema": { "type": "string", "pattern": "^[0-9]{1,19}$" },
      "description": description
   })
}

fn with_fields(mut parameters: Vec<Value>) -> Vec<Value> {
   parameters.push(json!({ "$ref": "#/components/parameters/TweetFields" }));
   parameters.push(json!({ "$ref": "#/components/parameters/UserFields" }));
   parameters
}

fn with_page(parameters: Vec<Value>) -> Vec<Value> {
   let mut parameters = with_fields(parameters);
   parameters.push(json!({ "$ref": "#/components/parameters/Cursor" }));
   parameters
}

fn with_count(mut parameters: Vec<Value>, maximum: u32) -> Vec<Value> {
   parameters.push(json!({
      "name": "count",
      "in": "query",
      "schema": { "type": "integer", "minimum": 1, "maximum": maximum }
   }));
   parameters
}

fn search_parameters() -> Vec<Value> {
   let mut parameters = vec![
      json!({
         "name": "q",
         "in": "query",
         "schema": { "type": "string", "maxLength": 512 },
         "description": "Search text. Operators such as from:name, since:YYYY-MM-DD, until:YYYY-MM-DD, min_faves:N, and filter:images work inside it. Required unless from or another filter is set. At most 512 characters."
      }),
      json!({
         "name": "f",
         "in": "query",
         "schema": { "type": "string", "enum": ["latest", "tweets", "top", "media", "users", "lists"] },
         "description": "Result tab. latest and tweets are the same. Default latest."
      }),
      json!({ "$ref": "#/components/parameters/Cursor" }),
      json!({ "$ref": "#/components/parameters/TweetFields" }),
      json!({ "$ref": "#/components/parameters/UserFields" }),
      json!({
         "name": "from",
         "in": "query",
         "schema": { "type": "string" },
         "description": "Only posts from these accounts. Commas separate handles."
      }),
      json!({
         "name": "since",
         "in": "query",
         "schema": { "type": "string", "format": "date" },
         "description": "First day to include, YYYY-MM-DD."
      }),
      json!({
         "name": "until",
         "in": "query",
         "schema": { "type": "string", "format": "date" },
         "description": "Last day to include, YYYY-MM-DD."
      }),
      json!({
         "name": "min_faves",
         "in": "query",
         "schema": { "type": "string" },
         "description": "Minimum likes."
      }),
      json!({
         "name": "filters",
         "in": "query",
         "schema": { "type": "string" },
         "description": "Comma-separated include filters, such as images,links. Same names as the f- toggles, without the prefix.",
         "example": "images,links"
      }),
      json!({
         "name": "exclude",
         "in": "query",
         "schema": { "type": "string" },
         "description": "Comma-separated exclude filters, such as replies,nativeretweets.",
         "example": "replies"
      }),
   ];
   for name in TOGGLES {
      let which = if name.starts_with("e-") { "Hide" } else { "Only" };
      parameters.push(json!({
         "name": name,
         "in": "query",
         "schema": { "type": "string", "enum": ["on", "true", "1", "yes"] },
         "description": format!("{which} posts matching {}.", name.split_once('-').map(|(_, rest)| rest).unwrap_or(name))
      }));
   }
   parameters
}

fn components() -> Value {
   let mut schemas = serde_json::Map::new();
   schemas.insert("Failure".into(), json!({
      "type": "object",
      "required": ["errors"],
      "properties": {
         "errors": { "type": "array", "items": { "$ref": "#/components/schemas/FailureItem" } }
      }
   }));
   schemas.insert("FailureItem".into(), json!({
      "type": "object",
      "required": ["code", "message"],
      "properties": {
         "code": { "type": "string" },
         "message": { "type": "string" }
      }
   }));
   schemas.insert("Meta".into(), json!({
      "type": "object",
      "required": ["result_count"],
      "properties": {
         "result_count": { "type": "integer", "minimum": 0 },
         "cursor": { "type": "string" },
         "query": { "type": "string", "description": "The search text sent upstream, after filters were applied." }
      }
   }));
   schemas.insert("PostPage".into(), page_schema("Post"));
   schemas.insert("AccountPage".into(), page_schema("Account"));
   schemas.insert("ListPage".into(), page_schema("List"));
   schemas.insert("IdPage".into(), json!({
      "type": "object",
      "required": ["data", "meta"],
      "properties": {
         "data": { "type": "array", "items": { "type": "string" } },
         "meta": { "$ref": "#/components/schemas/Meta" }
      }
   }));
   schemas.insert("PostEnvelope".into(), envelope("Post"));
   schemas.insert("AccountEnvelope".into(), envelope("Account"));
   schemas.insert("AboutEnvelope".into(), envelope("About"));
   schemas.insert("Account".into(), account_schema());
   schemas.insert("Post".into(), post_schema());
   schemas.insert("Metrics".into(), json!({
      "type": "object",
      "description": "A missing count is zero.",
      "properties": {
         "replies": { "type": "integer" },
         "reposts": { "type": "integer" },
         "likes": { "type": "integer" },
         "quotes": { "type": "integer" },
         "views": { "type": "integer" }
      }
   }));
   schemas.insert("Media".into(), json!({
      "type": "object",
      "required": ["type", "url"],
      "properties": {
         "type": { "type": "string", "enum": ["photo", "video", "gif"] },
         "url": { "type": "string" },
         "preview_url": { "type": "string" },
         "width": { "type": "integer" },
         "height": { "type": "integer" },
         "alt": { "type": "string" },
         "duration_ms": { "type": "integer" },
         "variants": {
            "type": "array",
            "description": "Present when tweet.fields includes variants.",
            "items": { "$ref": "#/components/schemas/Variant" }
         }
      }
   }));
   schemas.insert("Variant".into(), json!({
      "type": "object",
      "required": ["content_type", "url"],
      "properties": {
         "content_type": { "type": "string" },
         "url": { "type": "string" },
         "bitrate": { "type": "integer" },
         "height": { "type": "integer" }
      }
   }));
   schemas.insert("Entity".into(), json!({
      "type": "object",
      "required": ["type", "start", "end"],
      "properties": {
         "type": { "type": "string", "enum": ["url", "mention", "hashtag", "symbol"] },
         "start": { "type": "integer", "description": "Byte offset into text." },
         "end": { "type": "integer", "description": "Byte offset into text." },
         "text": { "type": "string" },
         "url": { "type": "string" }
      }
   }));
   schemas.insert("Card".into(), json!({
      "type": "object",
      "required": ["type"],
      "properties": {
         "type": { "type": "string" },
         "url": { "type": "string" },
         "title": { "type": "string" },
         "description": { "type": "string" },
         "image": { "type": "string" }
      }
   }));
   schemas.insert("Poll".into(), json!({
      "type": "object",
      "required": ["options", "votes"],
      "properties": {
         "options": { "type": "array", "items": { "$ref": "#/components/schemas/Choice" } },
         "votes": { "type": "integer" },
         "ends_at": { "type": "string", "format": "date-time" }
      }
   }));
   schemas.insert("Choice".into(), json!({
      "type": "object",
      "required": ["label", "votes"],
      "properties": {
         "label": { "type": "string" },
         "votes": { "type": "integer" }
      }
   }));
   schemas.insert("List".into(), json!({
      "type": "object",
      "required": ["id", "name"],
      "properties": {
         "id": { "type": "string" },
         "name": { "type": "string" },
         "description": { "type": "string" },
         "username": { "type": "string" },
         "members": { "type": "integer" },
         "subscribers": { "type": "integer" }
      }
   }));
   schemas.insert("About".into(), json!({
      "type": "object",
      "properties": {
         "account_based_in": { "type": "string" },
         "connection_source": { "type": "string" },
         "location_accurate": { "type": "boolean" }
      }
   }));

   json!({
      "securitySchemes": {
         "bearerAuth": {
            "type": "http",
            "scheme": "bearer",
            "description": "Search-pass API key. The token is tpk_ plus 64 hex digits. Create it on /search-pass. It is shown once."
         },
         "apiKey": {
            "type": "apiKey",
            "in": "header",
            "name": "X-Api-Key",
            "description": "The same API key, without the Bearer prefix."
         },
         "cookieAuth": {
            "type": "apiKey",
            "in": "cookie",
            "name": "paid_search_pass",
            "description": "Session cookie from signing in on /search-pass/login."
         }
      },
      "parameters": {
         "Cursor": {
            "name": "cursor",
            "in": "query",
            "schema": { "type": "string" },
            "description": "meta.cursor from the previous page. Omitted by the server on the last page."
         },
         "TweetFields": {
            "name": "tweet.fields",
            "in": "query",
            "schema": { "type": "string" },
            "description": "Comma-separated post fields. The default is created_at, lang, author, metrics, reply, media, quote, repost, sensitive. id and text are always included. Naming fields replaces the default. all selects every field. Extra names: entities, card, poll, source, location, note, history, variants, labels.",
            "example": "created_at,author,metrics"
         },
         "UserFields": {
            "name": "user.fields",
            "in": "query",
            "schema": { "type": "string" },
            "description": "Comma-separated profile fields added to id, username, and name: verified, avatar, banner, bio, location, website, followers, following, posts, likes, media, joined, pinned, protected, suspended. all selects every field. Profile lookups already return the full profile.",
            "example": "verified,followers"
         }
      },
      "responses": {
         "Error": {
            "description": "The call did not return results. code is pass_required (401), paused (429), invalid (400), blocked (400), not_found (404), suspended (403), protected (403), rate_limited (503), or unavailable (503).",
            "content": {
               "application/json": {
                  "schema": { "$ref": "#/components/schemas/Failure" }
               }
            }
         }
      },
      "schemas": schemas,
   })
}

fn account_schema() -> Value {
   json!({
      "type": "object",
      "required": ["id", "username"],
      "properties": {
         "id": { "type": "string" },
         "username": { "type": "string" },
         "name": { "type": "string" },
         "verified": { "type": "string", "enum": ["blue", "business", "government"] },
         "avatar_url": { "type": "string" },
         "banner_url": { "type": "string" },
         "bio": { "type": "string" },
         "location": { "type": "string" },
         "website": { "type": "string" },
         "followers": { "type": "integer" },
         "following": { "type": "integer" },
         "posts": { "type": "integer" },
         "likes": { "type": "integer" },
         "media": { "type": "integer" },
         "joined_at": { "type": "string", "format": "date-time" },
         "pinned_tweet_id": { "type": "string" },
         "protected": { "type": "boolean" },
         "suspended": { "type": "boolean" }
      }
   })
}

fn post_schema() -> Value {
   json!({
      "type": "object",
      "required": ["id", "text"],
      "properties": {
         "id": { "type": "string" },
         "text": { "type": "string" },
         "created_at": { "type": "string", "format": "date-time" },
         "lang": { "type": "string" },
         "author": { "$ref": "#/components/schemas/Account" },
         "metrics": { "$ref": "#/components/schemas/Metrics" },
         "reply_to": { "type": "array", "items": { "type": "string" } },
         "in_reply_to_status_id": { "type": "string" },
         "conversation_id": { "type": "string" },
         "sensitive": { "type": "boolean" },
         "unavailable": { "type": "string" },
         "pinned": { "type": "boolean" },
         "reposted_by": { "$ref": "#/components/schemas/Account" },
         "repost_id": { "type": "string" },
         "quote": { "$ref": "#/components/schemas/Post" },
         "media": { "type": "array", "items": { "$ref": "#/components/schemas/Media" } },
         "entities": { "type": "array", "items": { "$ref": "#/components/schemas/Entity" } },
         "card": { "$ref": "#/components/schemas/Card" },
         "poll": { "$ref": "#/components/schemas/Poll" },
         "source": { "type": "string" },
         "location": { "type": "string" },
         "note": { "type": "string" },
         "history": { "type": "array", "items": { "type": "string" } },
         "paid_promotion": { "type": "boolean" },
         "ai_generated": { "type": "boolean" }
      }
   })
}


fn page_schema(item: &str) -> Value {
   json!({
      "type": "object",
      "required": ["data", "meta"],
      "properties": {
         "data": {
            "type": "array",
            "items": { "$ref": format!("#/components/schemas/{item}") }
         },
         "meta": { "$ref": "#/components/schemas/Meta" }
      }
   })
}

fn envelope(item: &str) -> Value {
   json!({
      "type": "object",
      "required": ["data"],
      "properties": {
         "data": { "$ref": format!("#/components/schemas/{item}") }
      }
   })
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn the_spec_names_every_pass_endpoint() {
      let doc = document("https://xitter.cf");
      assert_eq!(doc["openapi"], "3.1.0");
      assert_eq!(doc["servers"][0]["url"], "https://xitter.cf");
      assert_eq!(doc["components"]["schemas"]["Post"]["properties"]["id"]["type"], "string");
      let paths = doc["paths"].as_object().unwrap();
      for path in [
         "/api/search",
         "/api/lists/{id}/tweets",
         "/api/communities/{id}/tweets",
         "/api/users/{username}",
         "/api/users/id/{id}",
         "/api/users/{username}/tweets",
         "/api/users/{username}/with_replies",
         "/api/users/{username}/about",
         "/api/users/{username}/following",
         "/api/users/{username}/following/ids",
         "/api/users/{username}/friends",
         "/api/users/{username}/followers",
         "/api/tweets/{id}",
         "/api/tweets",
         "/api/tweets/{id}/retweeters",
      ] {
         assert!(paths.contains_key(path), "{path}");
         assert!(paths[path]["get"]["operationId"].is_string());
      }
      let search = &paths["/api/search"]["get"]["parameters"];
      let names = search
         .as_array()
         .unwrap()
         .iter()
         .filter_map(|item| item["name"].as_str())
         .collect::<Vec<_>>();
      assert!(names.contains(&"q"));
      assert!(names.contains(&"f-images"));
      assert!(names.contains(&"e-replies"));
      assert!(names.contains(&"filters"));
   }
}
