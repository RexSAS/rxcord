use crate::id::Id;
use crate::model::{Channel, Guild, Message, MessageUpdate, Ready};
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Ready(Box<Ready>),
    Resumed,
    GuildCreate(Guild),
    /// `unavailable`: an outage, not a leave; the guild comes back with GUILD_CREATE.
    GuildDelete { id: Id, unavailable: bool },
    ChannelCreate(Channel),
    ChannelUpdate(Channel),
    ChannelDelete(Channel),
    MessageCreate(Box<Message>),
    MessageUpdate(MessageUpdate),
    MessageDelete { id: Id, channel_id: Id },
    MessageAck { channel_id: Id, message_id: Id },
    Unknown(String),
}

#[derive(Deserialize)]
struct GuildGone {
    id: Id,
    #[serde(default, deserialize_with = "crate::model::null_as_default")]
    unavailable: bool,
}

#[derive(Deserialize)]
struct Del {
    id: Id,
    channel_id: Id,
}

#[derive(Deserialize)]
struct Ack {
    channel_id: Id,
    message_id: Id,
}

pub fn parse_dispatch(name: &str, data: &RawValue) -> Result<Event, serde_json::Error> {
    let d = data.get();
    Ok(match name {
        "READY" => Event::Ready(Box::new(serde_json::from_str(d)?)),
        "RESUMED" => Event::Resumed,
        "GUILD_CREATE" => Event::GuildCreate(serde_json::from_str(d)?),
        "GUILD_DELETE" => {
            let x: GuildGone = serde_json::from_str(d)?;
            Event::GuildDelete { id: x.id, unavailable: x.unavailable }
        }
        "CHANNEL_CREATE" => Event::ChannelCreate(serde_json::from_str(d)?),
        "CHANNEL_UPDATE" => Event::ChannelUpdate(serde_json::from_str(d)?),
        "CHANNEL_DELETE" => Event::ChannelDelete(serde_json::from_str(d)?),
        "MESSAGE_CREATE" => Event::MessageCreate(Box::new(serde_json::from_str(d)?)),
        "MESSAGE_UPDATE" => Event::MessageUpdate(serde_json::from_str(d)?),
        "MESSAGE_DELETE" => {
            let x: Del = serde_json::from_str(d)?;
            Event::MessageDelete { id: x.id, channel_id: x.channel_id }
        }
        "MESSAGE_ACK" => {
            let x: Ack = serde_json::from_str(d)?;
            Event::MessageAck { channel_id: x.channel_id, message_id: x.message_id }
        }
        other => Event::Unknown(other.to_string()),
    })
}
