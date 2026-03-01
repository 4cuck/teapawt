//! Entity expansion for tweet and user bio text.
//!
//! Converts @mentions, #hashtags, and URLs into clickable HTML links.

use std::{
   borrow::Cow,
   fmt::Write as _,
   sync::LazyLock,
};

use regex::Regex;

use crate::types::{
   Entity,
   EntityKind,
};

static MENTION_RE: LazyLock<Regex> =
   LazyLock::new(|| Regex::new("(^|[^A-Za-z0-9_])@([A-Za-z0-9_]{1,15})").unwrap());
static HASHTAG_RE: LazyLock<Regex> =
   LazyLock::new(|| Regex::new(r"(^|[^\w&;])([#$])(\w+)").unwrap());

/// Convert character index to byte index.
/// Twitter entities use character (code point) indices, not byte indices.
fn char_to_byte_index(text: &str, char_idx: usize) -> Option<usize> {
   text
      .char_indices()
      .nth(char_idx)
      .map(|(byte_idx, _)| byte_idx)
      .or_else(|| {
         // If char_idx equals the number of chars, return the byte length
         (char_idx == text.chars().count()).then_some(text.len())
      })
}

/// Expand entities in text to HTML links.
///
/// Takes the original text and a list of entities with their positions,
/// then reconstructs the text with HTML anchor tags.
pub fn expand_entities(text: &str, entities: &[Entity]) -> String {
   if entities.is_empty() {
      return html_escape(text).into_owned();
   }

   // Sort entities by start position
   let mut sorted_entities: Vec<&Entity> = entities.iter().collect();
   sorted_entities.sort_by_key(|ent| ent.indices.0);

   // Deduplicate entities at same position
   sorted_entities.dedup_by(|ent_a, ent_b| ent_a.indices.0 == ent_b.indices.0);

   let mut result = String::with_capacity(text.len() * 2);
   let mut last_end_char = 0_usize;

   let char_count = text.chars().count();

   for entity in sorted_entities {
      let (start_char, end_char) = entity.indices;

      // Validate character indices
      if start_char > char_count || end_char > char_count || start_char >= end_char {
         continue;
      }

      // Convert character indices to byte indices
      let Some(start_byte) = char_to_byte_index(text, start_char) else {
         continue;
      };
      let Some(end_byte) = char_to_byte_index(text, end_char) else {
         continue;
      };
      let Some(last_end_byte) = char_to_byte_index(text, last_end_char) else {
         continue;
      };

      // Add text before this entity (escaped)
      if start_char > last_end_char {
         result.push_str(&html_escape(&text[last_end_byte..start_byte]));
      }

      // Add the entity as an HTML link
      match entity.kind {
         EntityKind::Mention => {
            let mention_text = &text[start_byte..end_byte];
            let _ = write!(
               result,
               r#"<a href="{}" title="{}">{}</a>"#,
               html_escape(&entity.url),
               html_escape(&entity.display),
               html_escape(mention_text)
            );
         },
         EntityKind::Hashtag => {
            let hashtag_text = &text[start_byte..end_byte];
            // Extract tag name without # symbol
            let tag_name = hashtag_text.trim_start_matches('#').trim_start_matches('$');
            let _ = write!(
               result,
               r#"<a href="/search?q=%23{}">{}</a>"#,
               url_encode(tag_name),
               html_escape(hashtag_text)
            );
         },
         EntityKind::Symbol => {
            let symbol_text = &text[start_byte..end_byte];
            let symbol_name = symbol_text.trim_start_matches('$');
            let _ = write!(
               result,
               r#"<a href="/search?q=%24{}">{}</a>"#,
               url_encode(symbol_name),
               html_escape(symbol_text)
            );
         },
         EntityKind::Url => {
            let display = short_url(&entity.url, 28);
            let _ = write!(
               result,
               r#"<a href="{}">{}</a>"#,
               html_escape(&entity.url),
               html_escape(&display)
            );
         },
      }

      last_end_char = end_char;
   }

   // Add remaining text after last entity (escaped)
   if last_end_char < char_count
      && let Some(last_end_byte) = char_to_byte_index(text, last_end_char)
   {
      result.push_str(&html_escape(&text[last_end_byte..]));
   }

   result
}

/// Regex-based expansion fallback.
pub fn expand_with_regex(text: &str) -> String {
   let escaped = html_escape(text);

   // Replace @mentions: @username -> link
   // Pattern: word boundary + @ + 1-15 alphanumeric/underscore chars
   let result = MENTION_RE.replace_all(&escaped, r#"$1<a href="/$2">@$2</a>"#);

   // Replace #hashtags: #topic -> link
   // Negative lookbehind (?<!;) prevents matching HTML entities like &#x27;
   // (which after html_escape becomes &amp;#x27; where ; is followed by #)
   let result = HASHTAG_RE.replace_all(&result, r#"$1<a href="/search?q=%23$3">$2$3</a>"#);

   result.into_owned()
}

/// HTML escape special characters.
pub fn html_escape(text: &str) -> Cow<'_, str> {
   // Fast path: check if any escaping is needed
   if !text
      .bytes()
      .any(|byte| matches!(byte, b'&' | b'<' | b'>' | b'"' | b'\''))
   {
      return Cow::Borrowed(text);
   }
   // Slow path: build escaped string
   let mut result = String::with_capacity(text.len() + text.len() / 8);
   for ch in text.chars() {
      match ch {
         '&' => result.push_str("&amp;"),
         '<' => result.push_str("&lt;"),
         '>' => result.push_str("&gt;"),
         '"' => result.push_str("&quot;"),
         '\'' => result.push_str("&#x27;"),
         _ => result.push(ch),
      }
   }
   Cow::Owned(result)
}

/// URL encode a string (delegates to formatters).
fn url_encode(text: &str) -> String {
   super::formatters::url_encode(text)
}

/// Strip protocol and `www.` from a URL, optionally truncating to `max_len`
/// characters with a Unicode ellipsis.
///
/// - `max_len == 0` → no truncation (profile website links)
/// - `max_len == 28` → standard short-link behaviour (entity display text)
pub fn short_url(url: &str, max_len: usize) -> String {
   let stripped = url
      .trim_start_matches("https://")
      .trim_start_matches("http://")
      .trim_start_matches("www.");

   if max_len == 0 {
      return stripped.to_owned();
   }

   let truncated: String = stripped.chars().take(max_len).collect();
   if truncated.len() < stripped.len() {
      format!("{truncated}\u{2026}")
   } else {
      truncated
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_expand_mention() {
      let entities = vec![Entity {
         indices: (0, 5),
         kind:    EntityKind::Mention,
         url:     "/test".to_string(),
         display: "Test User".to_string(),
      }];
      let result = expand_entities("@test hello", &entities);
      assert!(result.contains(r#"<a href="/test""#));
      assert!(result.contains("@test"));
   }

   #[test]
   fn test_expand_hashtag() {
      let entities = vec![Entity {
         indices: (0, 5),
         kind:    EntityKind::Hashtag,
         url:     String::new(),
         display: String::new(),
      }];
      let result = expand_entities("#rust is great", &entities);
      assert!(result.contains(r#"href="/search?q=%23rust""#));
   }

   #[test]
   fn test_expand_url() {
      let entities = vec![Entity {
         indices: (6, 26),
         kind: EntityKind::Url,
         url: "https://example.com/long/path".to_string(),
         ..Default::default()
      }];
      let result = expand_entities("Check https://t.co/xyz out!", &entities);
      assert!(result.contains(r#"href="https://example.com/long/path""#));
      // Display text is computed by short_url() at render time
      assert!(result.contains("example.com/long/path"));
   }

   #[test]
   fn test_html_escape() {
      let result = html_escape("<script>alert('xss')</script>");
      assert!(!result.contains('<'));
      assert!(!result.contains('>'));
      assert!(result.contains("&lt;"));
      assert!(result.contains("&gt;"));
   }

   #[test]
   fn test_regex_fallback() {
      let result = expand_with_regex("Hello @user and #topic!");
      assert!(result.contains(r#"<a href="/user">@user</a>"#));
      assert!(result.contains(r#"<a href="/search?q=%23topic">#topic</a>"#));
   }

   #[test]
   fn test_html_entity_not_treated_as_hashtag() {
      // &#x27; is the HTML entity for single quote -- the #x27 must NOT become a
      // hashtag link
      let result = expand_with_regex("It's a test");
      // After html_escape, ' becomes &#x27; -- should NOT contain a hashtag link for
      // #x27
      assert!(
         result.contains("&#x27;"),
         "HTML entity &#x27; should be preserved"
      );
      assert!(
         !result.contains(r#"<a href="/search?q=%23x27">"#),
         "&#x27; should not be treated as hashtag #x27"
      );
   }

   #[test]
   fn test_html_entity_ampersand_not_treated_as_hashtag() {
      // &amp;#x27; case (double-encoded)
      let result = expand_with_regex("Test &amp; &#x27;end");
      assert!(
         !result.contains(r#"<a href="/search?q=%23x27">"#),
         "&amp;#x27; should not produce hashtag link"
      );
   }
}
