//! Image thumbnails: URL sizing, decoding, a byte-capped LRU, and the fetcher the runtime drives.
use crate::view::MessageRow;
use lru::LruCache;
use rxcord_api::Rest;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::task::JoinSet;

pub const IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Longest side of a thumbnail, in px.
pub const THUMB_MAX: u32 = 400;
const MAX_FETCHES: usize = 4;
/// Download cap: thumbnails are requested at <= 400 px, so this only stops a hostile or broken server.
const MAX_DOWNLOAD: usize = 20 * 1024 * 1024;
/// Decode limits (decompression bombs): longest side, and bytes the decoder may allocate.
const MAX_DECODE_SIDE: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;

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

    /// Evicts least recently used images until under the cap and returns their keys; an image larger than
    /// the cap is not stored.
    pub fn insert(&mut self, key: String, buf: Pixels) -> Vec<String> {
        let c = cost(&buf);
        if c > self.cap {
            return vec![];
        }
        self.bytes += c;
        if let Some(old) = self.map.put(key, buf) {
            self.bytes -= cost(&old);
        }
        let mut evicted = vec![];
        while self.bytes > self.cap {
            let Some((k, old)) = self.map.pop_lru() else { break };
            self.bytes -= cost(&old);
            evicted.push(k);
        }
        evicted
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

/// The media proxy resizes on request: `width`/`height` fitting `max` (`max` x `max` if the size is unknown).
pub fn thumb_url(proxy_url: &str, w: Option<u32>, h: Option<u32>, max: u32) -> String {
    let (w, h) = fit(w, h, max).unwrap_or((max, max));
    format!("{proxy_url}{}width={w}&height={h}", if proxy_url.contains('?') { '&' } else { '?' })
}

/// PNG/JPEG/WebP/GIF (first frame) to RGBA, downscaled to fit `max` if the server sent it bigger.
pub fn decode(bytes: &[u8], max: u32) -> Option<Pixels> {
    decode_limited(bytes, max, MAX_DECODE_ALLOC)
}

fn decode_limited(bytes: &[u8], max: u32, max_alloc: u64) -> Option<Pixels> {
    let mut r = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(max_alloc);
    r.limits(limits);
    let mut img = r.decode().ok()?.into_rgba8();
    if let Some((w, h)) = fit(Some(img.width()), Some(img.height()), max).filter(|&s| s != img.dimensions()) {
        img = image::imageops::thumbnail(&img, w, h);
    }
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

pub type Loaded = (String, Option<Pixels>);

/// What the rows should change: show `show` where its URL is displayed, drop the images of `evicted` URLs
/// (they come back through the cache or a refetch when their row is created again).
#[derive(Default)]
pub struct ThumbUpdate {
    pub show: Option<(String, Pixels)>,
    pub evicted: Vec<String>,
}

fn shown_has(shown: &[MessageRow], url: &str) -> bool {
    shown.iter().any(|r| r.thumbs.iter().any(|t| t.url == url))
}

/// Thumbnail fetches: one per URL at a time, at most `MAX_FETCHES` running, failures not retried until
/// `forget_failures`. Queued URLs are dropped if no shown row displays them when their turn comes.
pub struct Thumbs {
    cache: ByteLru,
    failed: HashSet<String>,
    /// queued or running
    in_flight: HashSet<String>,
    queue: VecDeque<String>,
    /// The runtime loop polls this and hands results to `loaded`.
    pub tasks: JoinSet<Loaded>,
    cdn: Arc<Rest>,
}

impl Thumbs {
    pub fn new() -> Self {
        Self {
            cache: ByteLru::new(IMAGE_CACHE_BYTES),
            failed: HashSet::new(),
            in_flight: HashSet::new(),
            queue: VecDeque::new(),
            tasks: JoinSet::new(),
            // `get_bytes` sends no token: CDN fetches need no session.
            cdn: Arc::new(Rest::new(String::new())),
        }
    }

    /// A row showing `url` appeared: the cached image to show, else a fetch is queued (unless already, or failed).
    pub fn request(&mut self, url: String, shown: &[MessageRow]) -> ThumbUpdate {
        if let Some(px) = self.cache.get(&url) {
            return ThumbUpdate { show: Some((url, px)), evicted: vec![] };
        }
        if !self.failed.contains(&url) && self.in_flight.insert(url.clone()) {
            self.queue.push_back(url);
            self.start(shown);
        }
        ThumbUpdate::default()
    }

    /// A fetch finished: cache it, and show it if a shown row still displays `url`.
    pub fn loaded(&mut self, (url, px): Loaded, shown: &[MessageRow]) -> ThumbUpdate {
        self.in_flight.remove(&url);
        self.start(shown);
        let Some(px) = px else {
            self.failed.insert(url);
            return ThumbUpdate::default();
        };
        let evicted = self.cache.insert(url.clone(), px.clone());
        ThumbUpdate { show: shown_has(shown, &url).then_some((url, px)), evicted }
    }

    /// After a reconnect: failed URLs are fetched again when their rows are created again.
    pub fn forget_failures(&mut self) {
        self.failed.clear();
    }

    fn start(&mut self, shown: &[MessageRow]) {
        while self.tasks.len() < MAX_FETCHES
            && let Some(url) = self.queue.pop_front()
        {
            if !shown_has(shown, &url) {
                self.in_flight.remove(&url); // stale (channel switched): not a failure
                continue;
            }
            let cdn = self.cdn.clone();
            self.tasks.spawn(async move {
                let px = match cdn.get_bytes(&url, MAX_DOWNLOAD).await {
                    Ok(b) => tokio::task::spawn_blocking(move || decode(&b, THUMB_MAX)).await.ok().flatten(),
                    Err(e) => {
                        tracing::debug!("thumbnail fetch failed: {e}");
                        None
                    }
                };
                (url, px)
            });
        }
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
        assert_eq!(c.insert("big".into(), px(10, 20)), ["b", "c"]); // 800 bytes: two must go, reported
        assert!(c.get("b").is_none() && c.get("c").is_none());
        assert!(c.get("d").is_some() && c.get("big").is_some());
        assert!(c.insert("d".into(), px(10, 10)).is_empty()); // replacing doesn't double count
        assert!(c.get("big").is_some());
    }

    #[test]
    fn oversize_not_stored() {
        let mut c = ByteLru::new(400);
        c.insert("a".into(), px(10, 10));
        assert!(c.insert("huge".into(), px(10, 11)).is_empty());
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
        assert_eq!(thumb_url(u, None, Some(50), 400), format!("{u}?width=400&height=400")); // bounded anyway
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
        image::RgbaImage::new(80, 40).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let p = decode(&png, 40).unwrap();
        assert_eq!((p.width(), p.height()), (40, 20));
        let p = decode(&png, 100).unwrap();
        assert_eq!((p.width(), p.height()), (80, 40));
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

    #[test]
    fn decode_limits_dimensions() {
        let png = |w, h| {
            let mut v = vec![];
            image::GrayImage::new(w, h).write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png).unwrap();
            v
        };
        assert!(decode(&png(MAX_DECODE_SIDE, 1), 400).is_some());
        assert!(decode(&png(MAX_DECODE_SIDE + 1, 1), 400).is_none());
        // a small PNG decoding to more than the allocation limit is refused (a cheap 1 MB stand-in for 64 MB)
        let mut v = vec![];
        image::RgbaImage::new(100, 100).write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png).unwrap();
        assert!(v.len() < 1000);
        assert!(decode_limited(&v, 400, 30_000).is_none()); // 40 KB decoded
        assert!(decode_limited(&v, 400, 1 << 20).is_some());
    }

    fn shown(url: &str) -> Vec<MessageRow> {
        vec![row(url)]
    }

    #[tokio::test]
    async fn dedupes_and_does_not_retry_failures() {
        let mut t = Thumbs::new();
        let url = "http://127.0.0.1:1/x.png".to_string(); // refused at once
        assert!(t.request(url.clone(), &shown(&url)).show.is_none());
        assert!(t.request(url.clone(), &shown(&url)).show.is_none());
        assert_eq!(t.tasks.len(), 1);
        let done = t.tasks.join_next().await.unwrap().unwrap();
        assert!(done.1.is_none());
        assert!(t.loaded(done, &shown(&url)).show.is_none());
        t.request(url.clone(), &shown(&url));
        assert!(t.tasks.is_empty());
        t.forget_failures();
        t.request(url.clone(), &shown(&url));
        assert_eq!(t.tasks.len(), 1);
    }

    #[tokio::test]
    async fn at_most_four_and_stale_queue_dropped() {
        let mut t = Thumbs::new();
        let urls: Vec<String> = (0..6).map(|i| format!("http://127.0.0.1:1/{i}.png")).collect();
        let rows: Vec<MessageRow> = urls.iter().map(|u| row(u)).collect();
        for u in &urls {
            t.request(u.clone(), &rows);
        }
        assert_eq!(t.tasks.len(), 4);
        let done = t.tasks.join_next().await.unwrap().unwrap();
        t.loaded(done, &[]); // channel switched: the queued two are skipped, not failed
        assert_eq!(t.tasks.len(), 3);
        t.request(urls[5].clone(), &rows);
        assert_eq!(t.tasks.len(), 4);
    }

    #[tokio::test]
    async fn caches_and_pushes_only_shown() {
        let mut t = Thumbs::new();
        assert!(t.loaded(("a".into(), Some(px(2, 2))), &shown("b")).show.is_none()); // not shown: cached only
        let hit = t.request("a".into(), &[]).show.map(|(u, p)| (u, p.width()));
        assert_eq!(hit, Some(("a".into(), 2)));
        assert!(t.loaded(("c".into(), Some(px(2, 2))), &shown("c")).show.is_some());
        assert!(t.tasks.is_empty());
    }

    #[tokio::test]
    async fn loaded_reports_evictions() {
        let mut t = Thumbs::new();
        let big = || Some(px(4096, 1024)); // 16 MiB each: the fifth evicts the first
        for k in ["a", "b", "c", "d"] {
            assert!(t.loaded((k.into(), big()), &[]).evicted.is_empty());
        }
        assert_eq!(t.loaded(("e".into(), big()), &[]).evicted, ["a"]);
    }
}
