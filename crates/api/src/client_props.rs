//! The single home of the client identity (headers, super properties, identify properties).
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub const CAPABILITIES: u64 = 30717; // UPDATE FROM OFFICIAL CLIENT
pub const CLIENT_BUILD_NUMBER: u64 = 380000; // UPDATE FROM OFFICIAL CLIENT
pub const CLIENT_VERSION: &str = "1.0.9200"; // UPDATE FROM OFFICIAL CLIENT
const ELECTRON_VERSION: &str = "37.6.0"; // UPDATE FROM OFFICIAL CLIENT
const CHROME_VERSION: &str = "138.0.7204.251"; // UPDATE FROM OFFICIAL CLIENT
const LOCALE: &str = "en-US";

pub fn user_agent() -> String {
    let platform = if cfg!(windows) {
        "Windows NT 10.0; Win64; x64"
    } else {
        "X11; Linux x86_64"
    };
    format!(
        "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) discord/{CLIENT_VERSION} Chrome/{CHROME_VERSION} Electron/{ELECTRON_VERSION} Safari/537.36"
    )
}

pub fn super_properties_json() -> Value {
    let os = if cfg!(windows) { "Windows" } else { "Linux" };
    json!({
        "os": os,
        "browser": "Discord Client",
        "release_channel": "stable",
        "client_version": CLIENT_VERSION,
        "os_version": "",
        "os_arch": "x64",
        "system_locale": LOCALE,
        "client_build_number": CLIENT_BUILD_NUMBER,
    })
}

pub fn super_properties_header() -> String {
    STANDARD.encode(super_properties_json().to_string())
}

/// Headers shared by REST and QR login.
pub fn base_headers() -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent", user_agent()),
        ("X-Super-Properties", super_properties_header()),
        ("X-Discord-Locale", LOCALE.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips() {
        let raw = STANDARD.decode(super_properties_header()).unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["browser"], "Discord Client");
        assert_eq!(v["client_build_number"], CLIENT_BUILD_NUMBER);
        assert!(user_agent().contains("Electron/"));
        assert_eq!(base_headers().len(), 3);
    }
}
