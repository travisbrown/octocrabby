use chrono::{DateTime, Utc};
use octocrab::models::User;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ExtendedUser {
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub base: User,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct UserInfo {
    pub login: String,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    pub name: Option<String>,
    #[serde(rename = "twitterUsername")]
    pub twitter_username: Option<String>,
}
