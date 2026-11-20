use super::tweet::parse_tweet_object;
use crate::{
   api::schema::{
      ConversationData,
      InstructionType,
   },
   error::{
      Error,
      Result,
   },
   types::{
      Chain,
      Conversation,
      PaginatedResult,
      Tweet,
   },
};

/// Parse a conversation from `TweetDetail` response.
pub fn parse_conversation(
   data: &ConversationData,
   tweet_id: &str,
   has_cursor: bool,
) -> Result<Conversation> {
   const MIN_REPLIES_FOR_CURSOR: usize = 20;

   // Single tweet result path
   if let Some(tweet_data) = data
      .tweet_result
      .as_ref()
      .and_then(|nested| nested.result.as_deref())
      && tweet_data.__typename.is_some()
   {
      let tweet = parse_tweet_object(tweet_data)?;
      return Ok(Conversation {
         tweet,
         ..Default::default()
      });
   }

   // Instructions path
   let raw_instructions = data
      .threaded_conversation_with_injections_v2
      .as_ref()
      .map(|conv| conv.instructions.as_slice())
      .filter(|instr| !instr.is_empty())
      .ok_or_else(|| Error::TweetNotFound("No instructions array".to_owned()))?;

   let mut main_tweet = Option::<Tweet>::None;
   let mut before = Chain::default();
   let mut after = Chain::default();
   let mut replies = PaginatedResult::<Chain>::default();
   let mut show_more = Option::<String>::None;

   // Parse tweet_id for matching
   let target_id = tweet_id.parse().unwrap_or(0);

   for instruction in raw_instructions {
      if instruction.instruction_type != Some(InstructionType::TimelineAddEntries) {
         continue;
      }

      for entry in instruction.entries.as_deref().unwrap_or_default() {
         let entry_id = entry.entry_id_str();

         // Skip promoted content and injected suggestions (who-to-follow, etc.)
         if entry_id.contains("promoted")
            || entry_id.starts_with("who-to-follow")
            || entry_id.starts_with("tweetdetailrelatedtweets")
            || entry_id.starts_with("label-")
         {
            continue;
         }
         if entry_id.starts_with("tweet-") {
            // Single tweet entry -- match by ID to determine main vs before
            if let Some(tweet) = entry
               .tweet_result()
               .and_then(|result| parse_tweet_object(result).ok())
            {
               if tweet.id == target_id {
                  main_tweet = Some(tweet);
               } else {
                  before.content.push(tweet);
               }
            }
         } else if entry_id.starts_with("tombstone") {
            // Tombstone entry -- unavailable tweet in conversation
            let tombstone_id = entry_id
               .split('-')
               .next_back()
               .and_then(|id_str| id_str.parse().ok())
               .unwrap_or(0);
            let tweet = Tweet {
               id: tombstone_id,
               available: false,
               tombstone: "This tweet is unavailable".to_owned(),
               ..Default::default()
            };
            if tombstone_id == target_id {
               main_tweet = Some(tweet);
            } else {
               before.content.push(tweet);
            }
         } else if entry_id.starts_with("conversationthread-") {
            // Thread/replies -- check for self-thread (after) vs other replies
            let mut chain = Chain::default();
            let mut is_self_thread = false;

            for item in entry.items() {
               let item_id = item.entry_id_str();

               // Skip promoted items inside threads
               if item_id.contains("promoted") {
                  continue;
               }

               // Check for "Show more" cursor in thread
               if item_id.contains("cursor-showmore") {
                  let cursor = item.cursor_value().unwrap_or_default();
                  chain.has_more = true;
                  chain.cursor = Some(cursor.to_owned());
                  continue;
               }

               if let Some(tweet_result) = item.tweet_result()
                  && let Ok(tweet) = parse_tweet_object(tweet_result)
               {
                  chain.content.push(tweet);
               }

               // Check if this is a self-thread
               if item.display_type() == Some("SelfThread") {
                  is_self_thread = true;
               }
            }

            if !chain.content.is_empty() {
               if is_self_thread {
                  after = chain;
               } else {
                  replies.content.push(chain);
               }
            }
         } else if entry_id.starts_with("cursor-") {
            // Pagination cursor
            if let Some(cursor_value) = entry.cursor_value() {
               if entry_id.contains("bottom") {
                  replies.bottom = Some(cursor_value.to_owned());
               } else if entry_id.contains("top") {
                  replies.top = Some(cursor_value.to_owned());
               } else if entry_id.contains("showmorethreads") {
                  show_more = Some(cursor_value.to_owned());
               }
            }
         }
      }
   }

   // Twitter always sends a bottom_cursor even when there are no more
   // replies. The first page returns ~25 reply chains when there are more
   // pages. Subsequent pages return about 36. Strip dead cursors when the reply
   // count is well below the typical first-page size.
   if !has_cursor && replies.content.len() < MIN_REPLIES_FOR_CURSOR {
      replies.bottom = None;
   }

   // Once the ranked replies run out X stops sending a bottom cursor and
   // folds the rest behind "Show more replies". That cursor pages the same
   // way, so it continues the list where the bottom cursor would; unlike a
   // bottom cursor it is only sent when there really is more, so it survives
   // the dead-cursor check above.
   if replies.bottom.is_none() {
      replies.bottom = show_more;
   }

   // Paginated requests (with cursor) don't include the main tweet in the
   // response. Twitter only returns the next batch of replies.
   let tweet = if has_cursor {
      main_tweet.unwrap_or_default()
   } else {
      main_tweet
         .ok_or_else(|| Error::TweetNotFound("Main tweet not found in conversation".to_owned()))?
   };

   Ok(Conversation {
      tweet,
      before,
      after,
      replies,
   })
}

/// Put a first page asked for by Likes or Recency in the shape X gives the
/// same post under Relevance: the posts above it oldest first, and the
/// author's continuation under it instead of among the replies.
///
/// Those orders return the same posts filed differently. Likes lists the
/// posts above the main one from the nearest up, which read as a thread
/// running backwards under an "earlier replies" link that led nowhere new.
/// Both return the author's continuation as an ordinary reply chain rather
/// than a `SelfThread`, so the next posts of a thread were shown as replies
/// to it, wherever the order happened to put them, and the thread itself
/// ended at the post being read.
pub fn settle_thread(conversation: &mut Conversation) {
   order_ancestors(&conversation.tweet, &mut conversation.before.content);
   // X marked the continuation itself.
   if !conversation.after.content.is_empty() {
      return;
   }
   if let Some(index) = continuation_at(conversation) {
      conversation.after = conversation.replies.content.remove(index);
   }
}

/// Turn the posts above `main` round when X listed them from the nearest up.
fn order_ancestors(main: &Tweet, before: &mut [Tweet]) {
   if listed_newest_first(main, before) {
      before.reverse();
   }
}

/// Whether the posts above `main` run from the nearest one up.
///
/// The post `main` answers belongs right above it, which settles the order
/// whenever that post is listed. Failing that the ids do: a reply is newer
/// than the post it answers. An unavailable post is not compared, since its
/// id may not be known, and keeps its place between its neighbours either
/// way.
fn listed_newest_first(main: &Tweet, before: &[Tweet]) -> bool {
   let &[ref first, .., ref last] = before else {
      return false;
   };
   if main.reply_id > 0 {
      if last.id == main.reply_id {
         return false;
      }
      if first.id == main.reply_id {
         return true;
      }
   }
   let posts = before
      .iter()
      .filter(|tweet| tweet.available && tweet.id > 0)
      .map(|tweet| tweet.id)
      .collect::<Vec<_>>();
   posts.len() > 1 && posts.is_sorted_by(|listed, next| listed > next)
}

/// The reply chain X shows under the main post as its continuation when it
/// is asked by Relevance, found among the replies of another order.
///
/// It is the author answering their own post directly, with nobody else in
/// the chain, in a conversation that is theirs from its first post down.
/// When they answered it more than once, it is the most recent answer.
///
/// Read off 898 posts, opening the ones with replies in both orders.
/// Relevance showed a continuation on 34, and the Likes page held that chain
/// every time. Where the author had answered several times, on 8 posts
/// counting a second sample of threads, Relevance showed the latest every
/// time. On 7 posts a chain by the author alone was not a continuation: 6
/// were inside a conversation someone else opened, where X lists a
/// self-reply with the other replies, and 1 answered a reply that is no
/// longer shown, which leaves the author's post on its own without it
/// answering the main one.
///
/// Held against 1,000 other posts by Likes: none of the 954 without a
/// continuation was given one, and of the 46 with one, 43 got the same
/// chain. Two got none: X had left the next posts out of that page, or
/// listed the thread above without its first post, which is then not taken
/// to be the author's. One got an earlier answer than Relevance shows,
/// because the latest of several was not on the first page.
fn continuation_at(conversation: &Conversation) -> Option<usize> {
   let main = &conversation.tweet;
   if main.id <= 0 || main.user.id.is_empty() || !own_so_far(conversation) {
      return None;
   }
   conversation
      .replies
      .content
      .iter()
      .enumerate()
      .filter(|&(_, chain)| {
         chain
            .content
            .first()
            .is_some_and(|first| first.reply_id == main.id)
            && chain
               .content
               .iter()
               .all(|tweet| tweet.user.id == main.user.id)
      })
      .max_by_key(|&(_, chain)| chain.content.first().map_or(0, |first| first.id))
      .map(|(index, _)| index)
}

/// Whether every post, from the one that opened the conversation down to the
/// main one, is the main post's author's. Posts X did not list are not taken
/// on trust: when the ones above do not reach the opening post, or one of
/// them is unavailable, the answer is no.
fn own_so_far(conversation: &Conversation) -> bool {
   let main = &conversation.tweet;
   let above = &conversation.before.content;
   let Some(opener) = above.first() else {
      return main.thread_id == main.id;
   };
   opener.id == main.thread_id
      && above
         .iter()
         .all(|tweet| tweet.available && tweet.user.id == main.user.id)
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::types::User;

   /// A post in the conversation that post 10 opened.
   fn post(id: i64, author: &str, reply_id: i64) -> Tweet {
      Tweet {
         id,
         reply_id,
         thread_id: 10,
         available: true,
         user: User {
            id: author.to_owned(),
            ..User::default()
         },
         ..Tweet::default()
      }
   }

   fn tombstone(id: i64) -> Tweet {
      Tweet {
         id,
         available: false,
         ..Tweet::default()
      }
   }

   fn chain(posts: Vec<Tweet>) -> Chain {
      Chain {
         content: posts,
         ..Chain::default()
      }
   }

   fn ids(posts: &[Tweet]) -> Vec<i64> {
      posts.iter().map(|tweet| tweet.id).collect()
   }

   /// The page X sends under Likes for the third post of a thread: the posts
   /// above it from the nearest up, and the next two posts as a reply chain
   /// among the replies.
   fn mid_thread_by_likes() -> Conversation {
      Conversation {
         tweet:   post(30, "author", 20),
         before:  chain(vec![post(20, "author", 10), post(10, "author", 0)]),
         after:   Chain::default(),
         replies: PaginatedResult {
            content: vec![
               chain(vec![post(90, "reader", 30), post(95, "author", 90)]),
               chain(vec![post(40, "author", 30), post(50, "author", 40)]),
               chain(vec![post(70, "other", 30)]),
            ],
            ..PaginatedResult::default()
         },
      }
   }

   #[test]
   fn the_posts_above_are_turned_round_and_the_continuation_leaves_the_replies() {
      let mut page = mid_thread_by_likes();
      settle_thread(&mut page);
      assert_eq!(ids(&page.before.content), [10, 20]);
      assert_eq!(ids(&page.after.content), [40, 50]);
      // The author answering a reader is a reply, not the thread.
      assert_eq!(page.replies.content.len(), 2);
      assert_eq!(ids(&page.replies.content[0].content), [90, 95]);
      assert_eq!(ids(&page.replies.content[1].content), [70]);
   }

   #[test]
   fn a_page_already_in_the_shape_relevance_gives_is_left_alone() {
      let mut page = mid_thread_by_likes();
      page.before.content.reverse();
      page.after = page.replies.content.remove(1);
      settle_thread(&mut page);
      assert_eq!(ids(&page.before.content), [10, 20]);
      assert_eq!(ids(&page.after.content), [40, 50]);
      assert_eq!(page.replies.content.len(), 2);
   }

   #[test]
   fn a_continuation_x_marked_keeps_the_authors_other_answers_as_replies() {
      let mut page = mid_thread_by_likes();
      page.after = chain(vec![post(99, "author", 30)]);
      settle_thread(&mut page);
      assert_eq!(ids(&page.after.content), [99]);
      assert_eq!(page.replies.content.len(), 3);
   }

   #[test]
   fn of_several_answers_by_the_author_the_latest_is_the_continuation() {
      let mut page = mid_thread_by_likes();
      // Likes puts the best liked first, whenever it was written.
      page
         .replies
         .content
         .insert(0, chain(vec![post(35, "author", 30)]));
      page
         .replies
         .content
         .push(chain(vec![post(38, "author", 30)]));
      settle_thread(&mut page);
      assert_eq!(ids(&page.after.content), [40, 50]);
      assert_eq!(page.replies.content.len(), 4);
      assert_eq!(ids(&page.replies.content[0].content), [35]);
      assert_eq!(ids(&page.replies.content[3].content), [38]);
   }

   #[test]
   fn a_self_reply_in_a_conversation_someone_else_opened_stays_a_reply() {
      let mut page = mid_thread_by_likes();
      page.before = chain(vec![post(20, "author", 10), post(10, "someone", 0)]);
      settle_thread(&mut page);
      assert_eq!(ids(&page.before.content), [10, 20]);
      assert!(page.after.content.is_empty());
      assert_eq!(page.replies.content.len(), 3);
   }

   #[test]
   fn an_answer_to_a_deleted_reply_is_not_the_thread() {
      let mut page = mid_thread_by_likes();
      // X drops the deleted reply and what is left is the author alone.
      page.replies.content[1] = chain(vec![post(40, "author", 33)]);
      settle_thread(&mut page);
      assert!(page.after.content.is_empty());
      assert_eq!(page.replies.content.len(), 3);
   }

   #[test]
   fn a_chain_someone_else_joined_stays_a_reply() {
      let mut page = mid_thread_by_likes();
      page.replies.content[1] = chain(vec![post(40, "author", 30), post(45, "reader", 40)]);
      settle_thread(&mut page);
      assert!(page.after.content.is_empty());
      assert_eq!(page.replies.content.len(), 3);
   }

   #[test]
   fn posts_x_did_not_list_are_not_assumed_to_be_the_authors() {
      // The posts above stop short of the one that opened the conversation.
      let mut page = mid_thread_by_likes();
      page.before = chain(vec![post(20, "author", 15)]);
      settle_thread(&mut page);
      assert!(page.after.content.is_empty());

      // One of them is gone.
      let mut page = mid_thread_by_likes();
      page.before = chain(vec![
         post(20, "author", 15),
         tombstone(15),
         post(10, "author", 0),
      ]);
      settle_thread(&mut page);
      assert_eq!(ids(&page.before.content), [10, 15, 20]);
      assert!(page.after.content.is_empty());

      // A reply whose parent is not shown at all.
      let mut page = mid_thread_by_likes();
      page.before = Chain::default();
      settle_thread(&mut page);
      assert!(page.after.content.is_empty());
   }

   #[test]
   fn the_post_that_opens_a_thread_gets_its_continuation() {
      let mut page = mid_thread_by_likes();
      page.tweet = post(10, "author", 0);
      page.before = Chain::default();
      page.replies.content = vec![
         chain(vec![post(70, "other", 10)]),
         chain(vec![post(20, "author", 10), post(30, "author", 20)]),
      ];
      settle_thread(&mut page);
      assert_eq!(ids(&page.after.content), [20, 30]);
      assert_eq!(page.replies.content.len(), 1);
   }

   #[test]
   fn the_post_answered_decides_the_order_when_ids_cannot() {
      let main = post(30, "author", 20);

      // The nearest post first, and the only other one unavailable.
      let mut above = vec![post(20, "author", 10), tombstone(0)];
      order_ancestors(&main, &mut above);
      assert_eq!(ids(&above), [0, 20]);

      // Already in order: the same two the other way round.
      let mut above = vec![tombstone(0), post(20, "author", 10)];
      order_ancestors(&main, &mut above);
      assert_eq!(ids(&above), [0, 20]);

      // The post answered is itself unavailable under a number that is not
      // its id, so the ids of the posts that are there decide.
      let mut above = vec![
         tombstone(7_000),
         post(15, "author", 10),
         post(10, "author", 0),
      ];
      order_ancestors(&main, &mut above);
      assert_eq!(ids(&above), [10, 15, 7_000]);

      // One post, or none readable, has no order to correct.
      let mut above = vec![post(20, "author", 10)];
      order_ancestors(&main, &mut above);
      assert_eq!(ids(&above), [20]);
      let mut above = vec![tombstone(7_000), tombstone(0)];
      order_ancestors(&main, &mut above);
      assert_eq!(ids(&above), [7_000, 0]);
   }
}
