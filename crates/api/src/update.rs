//! Release checks against GitHub (no Discord involved, no token sent).
use crate::rest::{ApiError, client_builder, read_capped};
use serde_json::Value;

pub const REPO: &str = "RexSAS/rxcord";

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: (u64, u64, u64),
    pub tag: String,
    /// The release web page (fallback when the update can't be installed).
    pub page: String,
    pub asset_url: String,
    /// `<asset>.sha256` uploaded next to the asset by CI.
    pub sha256_url: Option<String>,
}

/// "v1.2.3" / "1.2.3" → (1, 2, 3); None for anything else (pre-releases included).
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut it = tag.strip_prefix('v').unwrap_or(tag).split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// The release in a `releases/latest` response, if it has a file named `asset`.
pub fn pick(json: &Value, asset: &str) -> Option<Release> {
    let tag = json["tag_name"].as_str()?;
    let assets = json["assets"].as_array()?;
    let url = |name: &str| {
        assets.iter().find(|a| a["name"] == name).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string)
    };
    Some(Release {
        version: parse_version(tag)?,
        tag: tag.to_string(),
        page: json["html_url"].as_str().unwrap_or_default().to_string(),
        asset_url: url(asset)?,
        sha256_url: url(&format!("{asset}.sha256")),
    })
}

fn client() -> reqwest::Client {
    // A plain client name: GitHub's API requires a User-Agent.
    client_builder().user_agent(concat!("rxcord/", env!("CARGO_PKG_VERSION"))).build().expect("http client")
}

/// The latest (non-pre-release) release of `REPO` carrying `asset`.
pub async fn latest(asset: &str) -> Result<Option<Release>, ApiError> {
    let resp = client().get(format!("https://api.github.com/repos/{REPO}/releases/latest")).send().await?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None); // no release yet
    }
    if !status.is_success() {
        return Err(ApiError::Http { status: status.as_u16(), body: String::new() });
    }
    let json: Value = serde_json::from_slice(&read_capped(resp, 1 << 20).await?)?;
    Ok(pick(&json, asset))
}

/// Downloads a release file, refusing more than `max` bytes.
pub async fn download(url: &str, max: usize) -> Result<Vec<u8>, ApiError> {
    let resp = client().get(url).send().await?;
    if !resp.status().is_success() {
        return Err(ApiError::Http { status: resp.status().as_u16(), body: String::new() });
    }
    read_capped(resp, max).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.10"), Some((0, 2, 10)));
        assert_eq!(parse_version("1.0.0"), Some((1, 0, 0)));
        for bad in ["v1.2", "v1.2.3-beta", "v1.2.3.4", "latest", ""] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn picks_platform_asset() {
        let j = json!({"tag_name": "v0.2.0", "html_url": "https://github.com/RexSAS/rxcord/releases/tag/v0.2.0", "assets": [
            {"name": "rxcord-linux-x86_64", "browser_download_url": "https://dl/linux"},
            {"name": "rxcord-linux-x86_64.sha256", "browser_download_url": "https://dl/linux.sha256"},
            {"name": "rxcord-windows-x86_64.exe", "browser_download_url": "https://dl/win"}]});
        let r = pick(&j, "rxcord-linux-x86_64").unwrap();
        assert_eq!((r.version, r.asset_url.as_str(), r.sha256_url.as_deref()), ((0, 2, 0), "https://dl/linux", Some("https://dl/linux.sha256")));
        assert_eq!(pick(&j, "rxcord-windows-x86_64.exe").unwrap().sha256_url, None);
        assert_eq!(pick(&j, "rxcord-macos"), None, "no file for this platform");
        assert_eq!(pick(&json!({"tag_name": "nightly", "assets": []}), "x"), None);
    }
}
