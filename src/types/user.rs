use serde::{
   Deserialize,
   Serialize,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifiedType {
   #[default]
   None,
   Blue,
   Business,
   Government,
}

#[expect(
   clippy::struct_field_names,
   reason = "user_pic matches the API field name"
)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
   pub id:            String,
   pub username:      String,
   pub fullname:      String,
   pub location:      String,
   pub website:       String,
   pub bio:           String,
   pub user_pic:      String,
   pub banner:        String,
   pub pinned_tweet:  i64,
   pub following:     i64,
   pub followers:     i64,
   pub tweets:        i64,
   pub likes:         i64,
   pub media:         i64,
   pub verified_type: VerifiedType,
   pub protected:     bool,
   pub suspended:     bool,
   #[serde(with = "time::serde::timestamp::option")]
   pub join_date:     Option<time::OffsetDateTime>,
}

impl User {
   pub const fn is_empty(&self) -> bool {
      self.id.is_empty()
   }
}
