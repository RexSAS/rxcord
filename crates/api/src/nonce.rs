const DISCORD_EPOCH_MS: u64 = 1420070400000;

pub fn make_nonce(unix_ms: u64) -> String {
    ((unix_ms - DISCORD_EPOCH_MS) << 22).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_is_snowflake_shaped() {
        assert_eq!(make_nonce(1420070400000 + 1), (1u64 << 22).to_string());
    }
}
