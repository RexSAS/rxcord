//! The single home of the client identity (headers, super properties, identify properties).
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

// Mimics the Discord web app in Firefox on Linux (values captured 2026-11-07).
// When Discord or Firefox updates, refresh CLIENT_BUILD_NUMBER and FIREFOX_VERSION from a browser capture.
// Bits 256 (auth token refresh) and 16384 (passive guild update v2) are cleared: rxcord implements
// neither. Re-enable only with support.
pub const CAPABILITIES: u64 = 1767421 & !(256 | 16384); // web client value, captured 2026-11-07
pub const CLIENT_BUILD_NUMBER: u64 = 667580;
const FIREFOX_VERSION: &str = "158.0";
const LOCALE: &str = "en-US";

pub fn user_agent() -> String {
    let platform = if cfg!(windows) { "Windows NT 10.0; Win64; x64" } else { "X11; Ubuntu; Linux x86_64" };
    format!("Mozilla/5.0 ({platform}; rv:{FIREFOX_VERSION}) Gecko/20100101 Firefox/{FIREFOX_VERSION}")
}

fn uuid_v4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Per-launch IDs: fixed for the life of the process, fresh on each start (like a page load).
fn launch_ids() -> &'static (String, String, String) {
    static IDS: std::sync::OnceLock<(String, String, String)> = std::sync::OnceLock::new();
    IDS.get_or_init(|| (uuid_v4(), uuid_v4(), uuid_v4()))
}

pub fn super_properties_json() -> Value {
    let os = if cfg!(windows) { "Windows" } else { "Linux" };
    let (launch_id, signature, hb_session) = launch_ids();
    json!({
        "os": os,
        "browser": "Firefox",
        "device": "",
        "device_model_name": "",
        "system_locale": LOCALE,
        "has_client_mods": false,
        "browser_user_agent": user_agent(),
        "browser_version": FIREFOX_VERSION,
        "os_version": if cfg!(windows) { "10" } else { "" },
        "referrer": "",
        "referring_domain": "",
        "referrer_current": "",
        "referring_domain_current": "",
        "release_channel": "stable",
        "client_build_number": CLIENT_BUILD_NUMBER,
        "client_event_source": null,
        "client_launch_id": launch_id,
        "launch_signature": signature,
        "client_heartbeat_session_id": hb_session,
        "client_app_state": "focused",
    })
}

pub fn super_properties_header() -> String {
    STANDARD.encode(super_properties_json().to_string())
}

/// Headers shared by REST and QR login.
pub fn base_headers() -> Vec<(&'static str, String)> {
    let tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into());
    vec![
        ("User-Agent", user_agent()),
        ("X-Super-Properties", super_properties_header()),
        ("X-Discord-Locale", LOCALE.to_string()),
        ("X-Discord-Timezone", tz),
        ("X-Debug-Options", "bugReporterEnabled".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips() {
        let raw = STANDARD.decode(super_properties_header()).unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["browser"], "Firefox");
        assert_eq!(v["client_launch_id"], super_properties_json()["client_launch_id"]);
        assert_eq!(v["client_build_number"], CLIENT_BUILD_NUMBER);
        assert!(user_agent().ends_with("Firefox/158.0"));
        assert_eq!(base_headers().len(), 5);
        assert_eq!(CAPABILITIES & (256 | 16384), 0);
        assert_eq!(CAPABILITIES, 1750781);
    }
}
