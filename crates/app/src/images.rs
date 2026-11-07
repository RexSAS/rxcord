//! Image thumbnails: URL sizing, decoding, a byte-capped LRU, and the fetcher the runtime drives.
use crate::view::MessageRow;
use lru::LruCache;
use rxcord_api::Rest;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub const IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Longest side of a thumbnail, in px.
pub const THUMB_MAX: u32 = 400;
const MAX_FETCHES: usize = 4;

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// LRU cache whose cap is the decoded size (`w*h*4` bytes) of its images.
pub struct ByteLru {
    map: LruCache<String, Pixels>,
    bytes: usize,
    cap: usize,
}

fn cost(b: &Pixels) -> usize {
    b.width() as usize * b.height() as usize * 4
}

impl ByteLru {
    pub fn new(cap_bytes: usize) -> Self {
        Self { map: LruCache::unbounded(), bytes: 0, cap: cap_bytes }
    }

    pub fn get(&mut self, key: &str) -> Option<Pixels> {
        self.map.get(key).cloned()
    }

    /// Evicts least recently used images until under the cap; an image larger than the cap is not stored.
    pub fn insert(&mut self, key: String, buf: Pixels) {
        let c = cost(&buf);
        if c > self.cap {
            return;
        }
        self.bytes += c;
        if let Some(old) = self.map.put(key, buf) {
            self.bytes -= cost(&old);
        }
        while self.bytes > self.cap {
            let Some((_, old)) = self.map.pop_lru() else { break };
            self.bytes -= cost(&old);
        }
    }
}

/// `(w, h)` scaled down to fit `max` x `max`, keeping the aspect ratio; `None` if a side is unknown or 0.
pub fn fit(w: Option<u32>, h: Option<u32>, max: u32) -> Option<(u32, u32)> {
    let (w, h) = (w.filter(|&w| w > 0)?, h.filter(|&h| h > 0)?);
    let long = w.max(h);
    if long <= max {
        return Some((w, h));
    }
    let scale = |x: u32| ((x as u64 * max as u64 / long as u64) as u32).max(1);
    Some((scale(w), scale(h)))
}

/// The media proxy resizes on request: `width`/`height` fitting `max`.
pub fn thumb_url(proxy_url: &str, w: Option<u32>, h: Option<u32>, max: u32) -> String {
    match fit(w, h, max) {
        Some((w, h)) => format!("{proxy_url}{}width={w}&height={h}", if proxy_url.contains('?') { '&' } else { '?' }),
        None => proxy_url.to_string(),
    }
}

/// PNG/JPEG/WebP/GIF (first frame) to RGBA, downscaled to fit `max` if the server sent it bigger.
pub fn decode(bytes: &[u8], max: u32) -> Option<Pixels> {
    let mut img = image::load_from_memory(bytes).ok()?.into_rgba8();
    if let Some((w, h)) = fit(Some(img.width()), Some(img.height()), max).filter(|&s| s != img.dimensions()) {
        img = image::imageops::thumbnail(&img, w, h);
    }
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

pub type Loaded = (String, Option<Pixels>);

/// Thumbnail fetches: one per URL at a time, at most `MAX_FETCHES` running, failures not retried this session.
pub struct Thumbs {
    cache: ByteLru,
    failed: HashSet<String>,
    in_flight: HashSet<String>,
    /// The runtime loop polls this and hands results to `loaded`.
    pub tasks: JoinSet<Loaded>,
    sem: Arc<Semaphore>,
    cdn: Arc<Rest>,
}

impl Thumbs {
    pub fn new() -> Self {
        Self {
            cache: ByteLru::new(IMAGE_CACHE_BYTES),
            failed: HashSet::new(),
            in_flight: HashSet::new(),
            tasks: JoinSet::new(),
            sem: Arc::new(Semaphore::new(MAX_FETCHES)),
            // `get_bytes` sends no token, so CDN fetches don't need (or outlive) a session.
            cdn: Arc::new(Rest::new(String::new())),
        }
    }

    /// A row showing `url` appeared: the cached image to push, else a fetch starts (unless running or failed).
    pub fn request(&mut self, url: String) -> Option<(String, Pixels)> {
        if let Some(px) = self.cache.get(&url) {
            return Some((url, px));
        }
        if self.failed.contains(&url) || !self.in_flight.insert(url.clone()) {
            return None;
        }
        let (sem, cdn) = (self.sem.clone(), self.cdn.clone());
        self.tasks.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let px = match cdn.get_bytes(&url).await {
                Ok(b) => tokio::task::spawn_blocking(move || decode(&b, THUMB_MAX)).await.ok().flatten(),
                Err(e) => {
                    tracing::debug!("thumbnail fetch failed: {e}");
                    None
                }
            };
            (url, px)
        });
        None
    }

    /// A fetch finished: cache it, and return it for pushing if a shown row still displays `url`.
    pub fn loaded(&mut self, (url, px): Loaded, shown: &[MessageRow]) -> Option<(String, Pixels)> {
        self.in_flight.remove(&url);
        let Some(px) = px else {
            self.failed.insert(url);
            return None;
        };
        self.cache.insert(url.clone(), px.clone());
        shown.iter().any(|r| r.thumbs.iter().any(|t| t.url == url)).then_some((url, px))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(w: u32, h: u32) -> Pixels {
        SharedPixelBuffer::new(w, h)
    }

    #[test]
    fn evicts_lru_over_cap() {
        let mut c = ByteLru::new(3 * 400); // three 10x10 images
        for k in ["a", "b", "c", "d"] {
            c.insert(k.into(), px(10, 10));
        }
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_some() && c.get("c").is_some() && c.get("d").is_some());
        c.insert("big".into(), px(10, 20)); // 800 bytes: two must go
        assert!(c.get("b").is_none() && c.get("c").is_none());
        assert!(c.get("d").is_some() && c.get("big").is_some());
        c.insert("d".into(), px(10, 10)); // replacing doesn't double count
        assert!(c.get("big").is_some());
    }

    #[test]
    fn oversize_not_stored() {
        let mut c = ByteLru::new(400);
        c.insert("a".into(), px(10, 10));
        c.insert("huge".into(), px(10, 11));
        assert!(c.get("huge").is_none());
        assert!(c.get("a").is_some());
    }

    #[test]
    fn get_refreshes_recency() {
        let mut c = ByteLru::new(2 * 400);
        c.insert("a".into(), px(10, 10));
        c.insert("b".into(), px(10, 10));
        assert!(c.get("a").is_some());
        c.insert("c".into(), px(10, 10));
        assert!(c.get("b").is_none());
        assert!(c.get("a").is_some() && c.get("c").is_some());
    }

    #[test]
    fn thumb_url_scales() {
        let u = "https://media.discordapp.net/attachments/1/2/a.png";
        assert_eq!(thumb_url(u, Some(800), Some(400), 400), format!("{u}?width=400&height=200"));
        assert_eq!(thumb_url(u, Some(300), Some(900), 400), format!("{u}?width=133&height=400"));
        assert_eq!(thumb_url(u, Some(100), Some(50), 400), format!("{u}?width=100&height=50")); // no upscale
        assert_eq!(thumb_url(u, None, Some(50), 400), u);
        assert_eq!(fit(Some(4000), Some(1), 400), Some((400, 1)));
        assert_eq!(fit(Some(0), Some(10), 400), None);
    }

    #[test]
    fn thumb_url_existing_query() {
        let u = "https://media.discordapp.net/a.png?ex=1&is=2";
        assert_eq!(thumb_url(u, Some(800), Some(400), 400), format!("{u}&width=400&height=200"));
    }

    #[test]
    fn decode_downscales_and_rejects_garbage() {
        let mut png = vec![];
        image::RgbaImage::new(800, 400).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let p = decode(&png, 400).unwrap();
        assert_eq!((p.width(), p.height()), (400, 200));
        let p = decode(&png, 1000).unwrap();
        assert_eq!((p.width(), p.height()), (800, 400));
        assert!(decode(b"not an image", 400).is_none());
    }

    fn row(thumb: &str) -> MessageRow {
        let t = crate::view::Thumb { url: thumb.into(), full: String::new(), width: 1, height: 1 };
        MessageRow {
            id: None,
            nonce: None,
            author: String::new(),
            time: String::new(),
            blocks: vec![],
            thumbs: vec![t],
            files: vec![],
            reply_preview: None,
            edited: false,
            mine: false,
            pending: false,
            failed: false,
        }
    }

    #[tokio::test]
    async fn dedupes_and_does_not_retry_failures() {
        let mut t = Thumbs::new();
        let url = "http://127.0.0.1:1/x.png".to_string(); // refused at once
        assert!(t.request(url.clone()).is_none());
        assert!(t.request(url.clone()).is_none());
        assert_eq!(t.tasks.len(), 1);
        let done = t.tasks.join_next().await.unwrap().unwrap();
        assert!(done.1.is_none());
        assert!(t.loaded(done, &[row(&url)]).is_none());
        assert!(t.request(url).is_none());
        assert!(t.tasks.is_empty());
    }

    #[tokio::test]
    async fn caches_and_pushes_only_shown() {
        let mut t = Thumbs::new();
        assert!(t.loaded(("a".into(), Some(px(2, 2))), &[row("b")]).is_none()); // not shown: cached only
        assert_eq!(t.request("a".into()).map(|(u, p)| (u, p.width())), Some(("a".into(), 2)));
        assert!(t.loaded(("c".into(), Some(px(2, 2))), &[row("c")]).is_some());
        assert!(t.tasks.is_empty());
    }
}
