//! Desktop notifications for live messages.
use rxcord_api::id::Id;
use rxcord_api::model::Message;
use rxcord_store::Store;

pub fn should_notify(msg: &Message, me: Id, is_dm: bool, selected: Option<Id>, window_focused: bool) -> bool {
    msg.author.id != me
        && (is_dm || msg.mention_everyone || msg.mentions.iter().any(|u| u.id == me))
        && !(window_focused && selected == Some(msg.channel_id))
}

pub fn notification_text(msg: &Message, store: &Store) -> (String, String) {
    let author = msg.author.display_name();
    let title = match store.channel(msg.channel_id) {
        Some(c) if c.guild_id.is_some() => format!("{author} in #{}", c.name.as_deref().unwrap_or("unknown")),
        _ => author.to_string(),
    };
    let body = if msg.content.is_empty() && !msg.attachments.is_empty() {
        "(attachment)".to_string()
    } else {
        msg.content.chars().take(200).collect()
    };
    (title, body)
}

/// Fire-and-forget: notify-rust does D-Bus I/O on Linux, so keep it off the runtime. Errors (no daemon) are only logged.
pub fn show(title: &str, body: &str) {
    let (t, b) = (title.to_owned(), body.to_owned());
    tokio::task::spawn_blocking(move || {
        if let Err(e) = notify_rust::Notification::new().appname("rxcord").summary(&t).body(&b).show() {
            tracing::debug!("notification failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxcord_api::event::Event;
    use rxcord_api::model::{Attachment, Ready, User};
    use rxcord_store::Change;

    const ME: Id = Id(1);
    const CH: Id = Id(100);

    fn user(id: u64, name: &str) -> User {
        serde_json::from_value(serde_json::json!({"id": id.to_string(), "username": name})).unwrap()
    }
    fn msg(id: u64, author: u64, content: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "channel_id": "100", "content": content, "timestamp": "t",
            "author": {"id": author.to_string(), "username": "bob"}
        }))
        .unwrap()
    }
    fn store(dm: bool) -> Store {
        let ch = serde_json::json!({"id": "100", "type": if dm {1} else {0}, "name": "general", "guild_id": if dm {None} else {Some("5")}});
        let r: Ready = serde_json::from_value(serde_json::json!({
            "user": {"id": "1", "username": "me"},
            "private_channels": if dm { vec![ch.clone()] } else { vec![] },
            "guilds": if dm { vec![] } else { vec![serde_json::json!({"id": "5", "name": "g", "channels": [ch]})] },
        }))
        .unwrap();
        let mut s = Store::default();
        s.apply(&Event::Ready(Box::new(r)));
        s
    }

    #[test]
    fn own_message_never() {
        let mut m = msg(10, 1, "hi");
        m.mention_everyone = true;
        assert!(!should_notify(&m, ME, true, None, false));
    }
    #[test]
    fn dm_notifies() {
        assert!(should_notify(&msg(10, 2, "hi"), ME, true, None, false));
    }
    #[test]
    fn guild_plain_message_silent() {
        assert!(!should_notify(&msg(10, 2, "hi"), ME, false, None, false));
    }
    #[test]
    fn mention_notifies() {
        let mut m = msg(10, 2, "hi");
        m.mentions = vec![user(1, "me")];
        assert!(should_notify(&m, ME, false, None, false));
        m.mentions.clear();
        m.mention_everyone = true;
        assert!(should_notify(&m, ME, false, None, false));
    }
    #[test]
    fn focused_selected_suppressed() {
        assert!(!should_notify(&msg(10, 2, "hi"), ME, true, Some(CH), true));
    }
    #[test]
    fn focused_other_channel_notifies() {
        assert!(should_notify(&msg(10, 2, "hi"), ME, true, Some(Id(7)), true));
    }
    #[test]
    fn unfocused_selected_notifies() {
        assert!(should_notify(&msg(10, 2, "hi"), ME, true, Some(CH), false));
    }

    #[test]
    fn text_dm_and_guild() {
        assert_eq!(notification_text(&msg(10, 2, "yo"), &store(true)), ("bob".into(), "yo".into()));
        assert_eq!(notification_text(&msg(10, 2, "yo"), &store(false)), ("bob in #general".into(), "yo".into()));
    }
    #[test]
    fn text_truncates_by_char_and_attachment_fallback() {
        let long = "é".repeat(300);
        assert_eq!(notification_text(&msg(10, 2, &long), &store(true)).1.chars().count(), 200);
        let mut m = msg(10, 2, "");
        m.attachments = vec![serde_json::from_value::<Attachment>(serde_json::json!({
            "id": "9", "filename": "a.png", "size": 1, "url": "u", "proxy_url": "u"
        }))
        .unwrap()];
        assert_eq!(notification_text(&m, &store(true)).1, "(attachment)");
    }
    #[test]
    fn redelivered_create_yields_no_unread() {
        // The runtime notifies only when apply reports Unread for the channel.
        let mut s = store(true);
        let m = msg(10, 2, "hi");
        assert!(s.apply(&Event::MessageCreate(Box::new(m.clone()))).contains(&Change::Unread(CH)));
        assert!(!s.apply(&Event::MessageCreate(Box::new(m))).contains(&Change::Unread(CH)));
    }

    /// Manual: needs a notification daemon. `cargo test -p rxcord -- --ignored`
    #[test]
    #[ignore]
    fn show_smoke() {
        notify_rust::Notification::new().summary("rxcord test").body("hello").show().unwrap();
    }
}
