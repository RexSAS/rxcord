use flate2::{Decompress, FlushDecompress};

const SUFFIX: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];

#[derive(Debug, thiserror::Error)]
pub enum InflateError {
    #[error("decompression failed: {0}")]
    Decompress(#[from] flate2::DecompressError),
    #[error("invalid utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

/// Inflates Discord `compress=zlib-stream` frames; one zlib context per connection.
pub struct Inflater {
    buf: Vec<u8>,
    z: Decompress,
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    pub fn new() -> Self {
        Self { buf: Vec::new(), z: Decompress::new(true) }
    }

    /// Returns `None` until a chunk completes a message (ends with the sync-flush suffix).
    pub fn push(&mut self, chunk: &[u8]) -> Option<Result<String, InflateError>> {
        self.buf.extend_from_slice(chunk);
        if !self.buf.ends_with(&SUFFIX) {
            return None;
        }
        let input = std::mem::take(&mut self.buf);
        Some(self.inflate(&input))
    }

    fn inflate(&mut self, input: &[u8]) -> Result<String, InflateError> {
        let mut out = Vec::with_capacity(input.len() * 4);
        let start = self.z.total_in();
        loop {
            let consumed = (self.z.total_in() - start) as usize;
            self.z.decompress_vec(&input[consumed..], &mut out, FlushDecompress::Sync)?;
            // Spare output room means zlib has flushed everything it had.
            if out.len() < out.capacity() {
                break;
            }
            out.reserve(out.capacity());
        }
        Ok(String::from_utf8(out)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    fn compress(c: &mut Compress, s: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len() + 64);
        c.compress_vec(s.as_bytes(), &mut out, FlushCompress::Sync).unwrap();
        assert!(out.ends_with(&SUFFIX));
        out
    }

    #[test]
    fn two_messages_shared_context() {
        let mut c = Compress::new(Compression::default(), true);
        let mut i = Inflater::new();
        for s in [r#"{"op":10,"d":{"heartbeat_interval":41250}}"#, r#"{"op":11}"#] {
            assert_eq!(i.push(&compress(&mut c, s)).unwrap().unwrap(), s);
        }
    }

    #[test]
    fn split_frame() {
        let mut c = Compress::new(Compression::default(), true);
        let msg = compress(&mut c, &"x".repeat(10_000));
        let (a, b) = msg.split_at(msg.len() / 2);
        let mut i = Inflater::new();
        assert!(i.push(a).is_none());
        assert_eq!(i.push(b).unwrap().unwrap().len(), 10_000);
    }
}
