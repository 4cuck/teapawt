use serde::Deserialize;

use super::timeline::parse_timeline_instructions;
use crate::{
   api::schema::{NestedResult, TimelineInstructions},
   error::Result,
   types::Timeline,
};

#[derive(Debug, Clone, Default)]
pub struct CommunityInfo {
   pub id: String,
   pub name: String,
   pub description: String,
   pub members: i64,
   pub banner: String,
   pub category: String,
   pub join_policy: String,
   pub rules: Vec<(String, String)>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CommunityQueryData {
   pub data: Option<CommunityDataField>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommunityDataField {
   #[serde(rename = "communityResults")]
   community_results: Option<NestedResult<CommunityNode>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommunityNode {
   #[serde(rename = "rest_id")]
   rest_id: Option<String>,
   id_str: Option<String>,
   name: Option<String>,
   description: Option<String>,
   #[serde(rename = "member_count")]
   member_count: Option<i64>,
   #[serde(rename = "join_policy")]
   join_policy: Option<String>,
   #[serde(rename = "primary_community_topic")]
   topic: Option<CommunityTopic>,
   #[serde(rename = "custom_banner_media")]
   banner: Option<CommunityBanner>,
   rules: Option<Vec<CommunityRuleNode>>,
   #[serde(rename = "ranked_community_timeline")]
   ranked: Option<CommunityTimelineBox>,
   #[serde(rename = "community_media_timeline")]
   media: Option<CommunityTimelineBox>,
   #[serde(rename = "community_filtered_timeline")]
   filtered: Option<CommunityTimelineBox>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommunityTopic {
   topic_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommunityBanner {
   #[serde(rename = "media_info")]
   media_info: Option<CommunityBannerInfo>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommunityBannerInfo {
   original_img_url: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommunityRuleNode {
   name: Option<String>,
   description: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommunityTimelineBox {
   timeline: Option<TimelineInstructions>,
}

pub fn parse_community(data: &CommunityQueryData) -> CommunityInfo {
   let Some(node) = data
      .data
      .as_ref()
      .and_then(|field| field.community_results.as_ref())
      .and_then(|nested| nested.result.as_deref())
   else {
      return CommunityInfo::default();
   };
   CommunityInfo {
      id: node
         .rest_id
         .clone()
         .or_else(|| node.id_str.clone())
         .unwrap_or_default(),
      name: node.name.clone().unwrap_or_default(),
      description: node.description.clone().unwrap_or_default(),
      members: node.member_count.unwrap_or(0),
      banner: node
         .banner
         .as_ref()
         .and_then(|banner| banner.media_info.as_ref())
         .and_then(|info| info.original_img_url.clone())
         .unwrap_or_default(),
      category: node
         .topic
         .as_ref()
         .and_then(|topic| topic.topic_name.clone())
         .unwrap_or_default(),
      join_policy: node.join_policy.clone().unwrap_or_default(),
      rules: node
         .rules
         .iter()
         .flatten()
         .filter_map(|rule| {
            let name = rule.name.clone().unwrap_or_default();
            if name.is_empty() {
               None
            } else {
               Some((name, rule.description.clone().unwrap_or_default()))
            }
         })
         .collect(),
   }
}

pub fn parse_community_timeline(data: &CommunityQueryData) -> Result<Timeline> {
   let Some(node) = data
      .data
      .as_ref()
      .and_then(|field| field.community_results.as_ref())
      .and_then(|nested| nested.result.as_deref())
   else {
      return Ok(Timeline::default());
   };
   let instructions = node
      .ranked
      .as_ref()
      .or(node.filtered.as_ref())
      .or(node.media.as_ref())
      .and_then(|timeline| timeline.timeline.as_ref())
      .map(|timeline| timeline.instructions.as_slice())
      .unwrap_or_default();
   parse_timeline_instructions(instructions)
}
