use crate::id::Id;
use serde::{Deserialize, Deserializer, Serialize};

/// Discord sometimes sends `null` for an empty list; treat it like a missing field.
pub(crate) fn null_as_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn unknown_user() -> String {
    "Unknown user".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: Id,
    #[serde(default = "unknown_user")]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: Id,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub position: i32,
    #[serde(default)]
    pub parent_id: Option<Id>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub recipient_ids: Vec<Id>,
    /// Full user objects: CHANNEL_CREATE for a new DM sends these instead of `recipient_ids`.
    #[serde(default, deserialize_with = "null_as_default", skip_serializing)]
    pub recipients: Vec<User>,
    #[serde(default)]
    pub last_message_id: Option<Id>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawGuild")]
pub struct Guild {
    pub id: Id,
    pub name: String,
    pub icon: Option<String>,
    pub channels: Vec<Channel>,
}

#[derive(Deserialize)]
struct RawGuild {
    id: Id,
    name: Option<String>,
    icon: Option<String>,
    properties: Option<GuildProps>,
    #[serde(default, deserialize_with = "null_as_default")]
    channels: Vec<Channel>,
}

#[derive(Deserialize)]
struct GuildProps {
    name: Option<String>,
    icon: Option<String>,
}

impl From<RawGuild> for Guild {
    fn from(r: RawGuild) -> Self {
        let (pname, picon) = r.properties.map(|p| (p.name, p.icon)).unwrap_or_default();
        Guild {
            id: r.id,
            name: r.name.or(pname).unwrap_or_default(),
            icon: r.icon.or(picon),
            channels: r.channels,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Id,
    pub filename: String,
    pub url: String,
    #[serde(default)]
    pub proxy_url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageRef {
    #[serde(default)]
    pub message_id: Option<Id>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub channel_id: Id,
    pub author: User,
    #[serde(default)]
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub attachments: Vec<Attachment>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default)]
    pub message_reference: Option<MessageRef>,
    #[serde(default)]
    pub referenced_message: Option<Box<Message>>,
    #[serde(default, deserialize_with = "nonce")]
    pub nonce: Option<String>,
}

fn nonce<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        S(String),
        I(i64),
    }
    Ok(Option::<N>::deserialize(d)?.map(|n| match n {
        N::S(s) => s,
        N::I(i) => i.to_string(),
    }))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageUpdate {
    pub id: Id,
    pub channel_id: Id,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadStateEntry {
    pub id: Id,
    #[serde(default)]
    pub last_message_id: Option<Id>,
    #[serde(default)]
    pub mention_count: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Ready {
    pub user: User,
    #[serde(default, deserialize_with = "null_as_default")]
    pub guilds: Vec<Guild>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub private_channels: Vec<Channel>,
    #[serde(default, deserialize_with = "read_state")]
    pub read_state: Vec<ReadStateEntry>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub users: Vec<User>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub resume_gateway_url: String,
}

fn read_state<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ReadStateEntry>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum R {
        List(Vec<ReadStateEntry>),
        Wrapped {
            #[serde(default, deserialize_with = "null_as_default")]
            entries: Vec<ReadStateEntry>,
        },
    }
    Ok(match Option::<R>::deserialize(d)? {
        Some(R::List(v) | R::Wrapped { entries: v }) => v,
        None => vec![],
    })
}
