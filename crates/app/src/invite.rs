//! Discord invite links in message text (`discord.gg/CODE`, `discord.com/invite/CODE`).
use rxcord_api::rest::valid_invite_code;

const HOSTS: [&str; 3] = ["discord.gg/", "discord.com/invite/", "discordapp.com/invite/"];
/// A message rarely carries more; more buttons would only clutter the row.
const MAX: usize = 3;

/// Invite codes in `text`, in order, without duplicates.
pub fn invite_codes(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for host in HOSTS {
        for (at, _) in text.match_indices(host) {
            // The host must start a word or URL: "notdiscord.gg/x" is someone else's site.
            let before = text[..at].chars().next_back();
            if before.is_some_and(|c| !(c.is_whitespace() || matches!(c, '/' | '<' | '(' | '[' | '"' | '\''))) {
                continue;
            }
            let rest = &text[at + host.len()..];
            let code: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
            if valid_invite_code(&code) && !out.contains(&code) {
                out.push(code);
            }
        }
    }
    out.truncate(MAX);
    out
}

#[cfg(test)]
mod tests {
    use super::invite_codes;

    #[test]
    fn finds_invites() {
        assert_eq!(invite_codes("join https://discord.gg/AbC-12 now"), ["AbC-12"]);
        assert_eq!(invite_codes("<https://discord.com/invite/xyz>, discord.gg/xyz again"), ["xyz"]);
        assert_eq!(invite_codes("discordapp.com/invite/old?x=1"), ["old"]);
        assert!(invite_codes("https://notdiscord.gg/evil and discord.gg/ and discord.gg/x").is_empty());
        assert_eq!(invite_codes("discord.gg/a1 discord.gg/a2 discord.gg/a3 discord.gg/a4").len(), 3);
    }
}
