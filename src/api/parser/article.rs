use std::collections::HashMap;

use crate::{
   api::schema::{
      InlineArticle,
      InlineContentState,
      TweetData,
   },
   error::{
      Error,
      Result,
   },
   types::{
      Article,
      ArticleBlockType,
      ArticleEntity,
      ArticleEntityRange,
      ArticleEntityType,
      ArticleMedia,
      ArticleMediaType,
      ArticleParagraph,
      ArticleStyle,
      ArticleStyleRange,
      User,
   },
};

use super::user::parse_user_object;

/// Parse an inline article from a tweet response fetched with article field
/// toggles.
#[expect(clippy::module_name_repetitions, reason = "public API entry point")]
pub fn parse_article(tweet_data: &TweetData) -> Result<Article> {
   let inline = tweet_data
      .article
      .as_ref()
      .and_then(|wrapper| wrapper.article_results.as_ref())
      .and_then(|nested| nested.result.as_deref())
      .ok_or_else(|| Error::NotFound("Article not found in tweet response".into()))?;

   let user = tweet_data
      .core
      .as_ref()
      .and_then(|core| core.user_value())
      .and_then(|user_data| parse_user_object(user_data).ok())
      .unwrap_or_default();

   Ok(parse_inline_article(inline, user))
}

fn parse_inline_article(raw: &InlineArticle, user: User) -> Article {
   let title = raw.title.clone().unwrap_or_default();

   let cover_image = raw
      .cover_media
      .as_ref()
      .and_then(|cm| cm.media_info.as_ref())
      .and_then(|mi| mi.original_img_url.clone())
      .unwrap_or_default();

   let time = raw
      .metadata
      .as_ref()
      .and_then(|meta| meta.first_published_at_secs)
      .and_then(|secs| time::OffsetDateTime::from_unix_timestamp(secs).ok());

   let (paragraphs, entities) = raw.content_state.as_ref().map_or_else(
      || (Vec::new(), Vec::new()),
      parse_content_state,
   );

   // Parse media
   let mut media = HashMap::new();
   if let Some(ref media_entries) = raw.media_entities {
      for entry in media_entries {
         let Some(ref id) = entry.media_id else {
            continue;
         };
         let Some(ref info) = entry.media_info else {
            continue;
         };

         let type_name = info.__typename.as_deref().unwrap_or("");
         let (media_type, url) = match type_name {
            "ApiImage" => (
               ArticleMediaType::ApiImage,
               info.original_img_url.clone().unwrap_or_default(),
            ),
            "ApiGif" => (
               ArticleMediaType::ApiGif,
               info
                  .variants
                  .as_ref()
                  .and_then(|vars| vars.first())
                  .and_then(|var| var.url.clone())
                  .unwrap_or_default(),
            ),
            _ => continue,
         };

         media.insert(
            id.clone(),
            ArticleMedia { media_type, url },
         );
      }
   }

   Article {
      title,
      cover_image,
      user,
      time,
      paragraphs,
      entities,
      media,
   }
}

fn parse_content_state(
   state: &InlineContentState,
) -> (Vec<ArticleParagraph>, Vec<ArticleEntity>) {
   let paragraphs: Vec<ArticleParagraph> = state
      .blocks
      .iter()
      .map(|block| {
         let base_type = match block.block_type.as_str() {
            "blockquote" => ArticleBlockType::Blockquote,
            "code-block" => ArticleBlockType::CodeBlock,
            "header-one" => ArticleBlockType::HeaderOne,
            "header-two" => ArticleBlockType::HeaderTwo,
            "header-three" => ArticleBlockType::HeaderThree,
            "ordered-list-item" => ArticleBlockType::OrderedListItem,
            "unordered-list-item" => ArticleBlockType::UnorderedListItem,
            "atomic" => ArticleBlockType::Atomic,
            _ => ArticleBlockType::Unstyled,
         };

         let inline_style_ranges = block
            .inline_style_ranges
            .iter()
            .map(|sr| {
               let style = match sr.style.as_str() {
                  "Bold" => ArticleStyle::Bold,
                  "Italic" => ArticleStyle::Italic,
                  "Strikethrough" => ArticleStyle::Strikethrough,
                  _ => ArticleStyle::Unknown,
               };
               ArticleStyleRange {
                  offset: sr.offset,
                  length: sr.length,
                  style,
               }
            })
            .collect();

         let entity_ranges = block
            .entity_ranges
            .iter()
            .map(|er| ArticleEntityRange {
               offset: er.offset,
               length: er.length,
               key:    er.key,
            })
            .collect();

         ArticleParagraph {
            text: block.text.clone(),
            base_type,
            inline_style_ranges,
            entity_ranges,
         }
      })
      .collect();

   // entity_map is now a list of {key, value} pairs — sort by numeric key
   let mut sorted_entries: Vec<_> = state
      .entity_map
      .iter()
      .filter_map(|entry| entry.key.parse::<usize>().ok().map(|idx| (idx, entry)))
      .collect();
   sorted_entries.sort_by_key(|&(idx, _)| idx);

   let mut entities = Vec::new();
   for &(_, entry) in &sorted_entries {
      let raw = &entry.value;
      let data = raw.data.as_ref();

      let entity_type = match raw.entity_type.as_str() {
         "LINK" => ArticleEntityType::Link,
         "MARKDOWN" => ArticleEntityType::Markdown,
         "MEDIA" => ArticleEntityType::Media,
         "TWEET" => ArticleEntityType::Tweet,
         "TWEMOJI" => ArticleEntityType::Twemoji,
         "DIVIDER" => ArticleEntityType::Divider,
         _ => ArticleEntityType::Unknown,
      };

      let url = data.and_then(|ed| ed.url.clone()).unwrap_or_default();
      let tweet_id = data
         .and_then(|ed| ed.tweet_id.clone())
         .unwrap_or_default();
      let twemoji = url.clone();
      let markdown = data
         .and_then(|ed| ed.markdown.clone())
         .unwrap_or_default();
      let media_ids = data
         .and_then(|ed| ed.media_items.as_ref())
         .map(|items| items.iter().map(|item| item.media_id.clone()).collect())
         .unwrap_or_default();

      entities.push(ArticleEntity {
         entity_type,
         url,
         media_ids,
         tweet_id,
         twemoji,
         markdown,
      });
   }

   (paragraphs, entities)
}
