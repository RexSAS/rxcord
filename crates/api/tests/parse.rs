use rxcord_api::event::{Event, parse_dispatch};
use serde_json::value::RawValue;

fn parse(name: &str, json: &str) -> Result<Event, serde_json::Error> {
    parse_dispatch(name, &RawValue::from_string(json.to_string()).unwrap())
}

fn fixture(name: &str, file: &str) -> Event {
    let s = std::fs::read_to_string(format!("tests/fixtures/{file}")).unwrap();
    parse(name, &s).unwrap()
}

#[test]
fn ready_legacy_parses() {
    let Event::Ready(r) = fixture("READY", "ready_legacy.json") else { panic!() };
    assert_eq!(r.guilds.len(), 1);
    assert_eq!(r.guilds[0].name, "Test Guild");
    assert_eq!(r.guilds[0].channels.len(), 2);
    assert_eq!(r.guilds[0].channels[0].kind, 0);
    assert_eq!(r.read_state.len(), 1);
    assert_eq!(r.read_state[0].mention_count, 2);
    assert_eq!(r.private_channels[0].recipient_ids.len(), 1);
    assert_eq!(r.user.display_name(), "Rex");
}

#[test]
fn ready_capabilities_parses() {
    let Event::Ready(r) = fixture("READY", "ready_capabilities.json") else { panic!() };
    assert_eq!(r.guilds[0].name, "Cap Guild");
    assert_eq!(r.guilds[0].icon.as_deref(), Some("capicon"));
    assert_eq!(r.read_state.len(), 2);
}

#[test]
fn message_full() {
    let Event::MessageCreate(m) = fixture("MESSAGE_CREATE", "message_create.json") else { panic!() };
    assert_eq!(m.attachments[0].width, Some(640));
    assert_eq!(m.mentions.len(), 1);
    assert!(m.message_reference.unwrap().message_id.is_some());
    assert_eq!(m.referenced_message.unwrap().content, "original");
    assert_eq!(m.id.to_string(), "400000000000000020");
}

#[test]
fn message_minimal_author() {
    let Event::MessageCreate(m) = fixture("MESSAGE_CREATE", "message_create_minimal_author.json") else { panic!() };
    assert_eq!(m.author.display_name(), "Unknown user");
}

#[test]
fn message_int_nonce() {
    let Event::MessageCreate(m) = fixture("MESSAGE_CREATE", "message_create_int_nonce.json") else { panic!() };
    assert_eq!(m.nonce, Some("1234".into()));
}

#[test]
fn unknown_event_is_unknown() {
    let e = fixture("SOME_NEW_THING", "unknown_event.json");
    assert!(matches!(e, Event::Unknown(n) if n == "SOME_NEW_THING"));
}

#[test]
fn malformed_known_event_errs() {
    assert!(parse("MESSAGE_CREATE", r#"{"id":5}"#).is_err());
}

#[test]
fn small_events() {
    assert!(matches!(parse("RESUMED", "{}").unwrap(), Event::Resumed));
    assert!(matches!(parse("GUILD_DELETE", r#"{"id":"7"}"#).unwrap(), Event::GuildDelete { id, unavailable: false } if id.0 == 7));
    assert!(matches!(parse("GUILD_DELETE", r#"{"id":"7","unavailable":true}"#).unwrap(), Event::GuildDelete { unavailable: true, .. }));
    assert!(matches!(parse("MESSAGE_DELETE", r#"{"id":"1","channel_id":"2"}"#).unwrap(),
        Event::MessageDelete { id, channel_id } if id.0 == 1 && channel_id.0 == 2));
    assert!(matches!(parse("MESSAGE_ACK", r#"{"channel_id":"2","message_id":"3","version":9}"#).unwrap(),
        Event::MessageAck { channel_id, message_id } if channel_id.0 == 2 && message_id.0 == 3));
    assert!(matches!(parse("MESSAGE_UPDATE", r#"{"id":"1","channel_id":"2","content":"x"}"#).unwrap(), Event::MessageUpdate(u) if u.content.as_deref() == Some("x")));
}

#[test]
fn captured_fixtures_parse() {
    let Ok(rd) = std::fs::read_dir("tests/fixtures/captured") else { return };
    for f in rd.map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")) {
        let stem = f.file_stem().unwrap().to_str().unwrap();
        let name = stem.split_once('_').unwrap().1;
        let s = std::fs::read_to_string(&f).unwrap();
        parse(name, &s).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
    }
}

#[test]
fn ready_null_collections_parse() {
    let r = r#"{"user":{"id":"1","username":"u"},"guilds":null,"private_channels":null,"read_state":null,"users":null}"#;
    let Event::Ready(r) = parse("READY", r).unwrap() else { panic!() };
    assert!(r.guilds.is_empty() && r.private_channels.is_empty() && r.read_state.is_empty() && r.users.is_empty());
    let r = r#"{"user":{"id":"1","username":"u"},"read_state":{"entries":null},
        "guilds":[{"id":"2","name":"g","channels":null}],
        "private_channels":[{"id":"3","type":1,"recipient_ids":null}]}"#;
    let Event::Ready(r) = parse("READY", r).unwrap() else { panic!() };
    assert!(r.guilds[0].channels.is_empty() && r.private_channels[0].recipient_ids.is_empty() && r.read_state.is_empty());
    let m = r#"{"id":"1","channel_id":"2","author":{"id":"3"},"timestamp":"t","mentions":null,"attachments":null}"#;
    let Event::MessageCreate(m) = parse("MESSAGE_CREATE", m).unwrap() else { panic!() };
    assert!(m.mentions.is_empty() && m.attachments.is_empty());
}

#[test]
fn voice_events_parse() {
    let Event::VoiceStateUpdate(v) = parse("VOICE_STATE_UPDATE", r#"{"user_id":"1","channel_id":"2","guild_id":null,"session_id":"s","self_mute":true,"self_deaf":false,"mute":false,"deaf":false}"#).unwrap() else { panic!() };
    assert_eq!((v.user_id.0, v.channel_id.map(|c| c.0), v.guild_id, v.self_mute), (1, Some(2), None, true));
    let Event::VoiceStateUpdate(v) = parse("VOICE_STATE_UPDATE", r#"{"user_id":"1","channel_id":null}"#).unwrap() else { panic!() };
    assert_eq!((v.channel_id, v.session_id.as_str()), (None, ""));
    let Event::VoiceServerUpdate(v) = parse("VOICE_SERVER_UPDATE", r#"{"token":"t","guild_id":null,"channel_id":"5","endpoint":null}"#).unwrap() else { panic!() };
    assert_eq!((v.endpoint, v.channel_id.map(|c| c.0)), (None, Some(5)));
}

#[test]
fn voice_server_debug_redacts() {
    let Event::VoiceServerUpdate(v) = parse("VOICE_SERVER_UPDATE", r#"{"token":"SECRETVOICE","endpoint":"e.discord.media:443"}"#).unwrap() else { panic!() };
    let d = format!("{v:?}");
    assert!(!d.contains("SECRETVOICE") && d.contains("e.discord.media"));
}

#[test]
fn call_events_parse() {
    let Event::CallCreate(c) = parse("CALL_CREATE", r#"{"channel_id":"9","message_id":"1","ringing":["2","3"],"voice_states":[{"user_id":"2","channel_id":"9"}]}"#).unwrap() else { panic!() };
    assert_eq!((c.ringing.len(), c.voice_states.len()), (2, 1));
    let Event::CallUpdate(c) = parse("CALL_UPDATE", r#"{"channel_id":"9","ringing":null}"#).unwrap() else { panic!() };
    assert!(c.ringing.is_empty() && c.voice_states.is_empty());
    assert!(matches!(parse("CALL_DELETE", r#"{"channel_id":"9"}"#).unwrap(), Event::CallDelete { channel_id } if channel_id.0 == 9));
}

#[test]
fn guild_voice_states_both_shapes() {
    let vs = r#"[{"user_id":"1","channel_id":"2"}]"#;
    let legacy = format!(r#"{{"id":"5","name":"G","voice_states":{vs}}}"#);
    let caps = format!(r#"{{"id":"5","properties":{{"name":"G"}},"voice_states":{vs}}}"#);
    for j in [legacy, caps] {
        let Event::GuildCreate(g) = parse("GUILD_CREATE", &j).unwrap() else { panic!() };
        assert_eq!(g.voice_states.len(), 1);
    }
    let Event::GuildCreate(g) = parse("GUILD_CREATE", r#"{"id":"5","name":"G","voice_states":null}"#).unwrap() else { panic!() };
    assert!(g.voice_states.is_empty());
}
