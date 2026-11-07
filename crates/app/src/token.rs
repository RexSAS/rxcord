//! Token storage in the OS keyring only. Blocking: call from `spawn_blocking`. Never logs the token.
use keyring::Entry;

fn entry() -> keyring::Result<Entry> {
    Entry::new("rxcord", "token")
}

pub fn load() -> Option<String> {
    match entry().and_then(|e| e.get_password()) {
        Ok(t) => Some(t),
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::warn!("keyring read failed: {e}");
            None
        }
    }
}

pub fn save(token: &str) {
    if let Err(e) = entry().and_then(|e| e.set_password(token)) {
        tracing::warn!("keyring write failed: {e}");
    }
}

pub fn delete() {
    match entry().and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(e) => tracing::warn!("keyring delete failed: {e}"),
    }
}
