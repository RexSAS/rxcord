//! Per-user reordering buffer for 20 ms Opus frames.

#[derive(Debug, PartialEq)]
pub enum Popped {
    Frame(Vec<u8>),
    Lost,
    Empty,
}

/// Frames buffered before playback starts (60 ms).
const TARGET: usize = 3;
/// Frames kept at most (200 ms); older ones are dropped.
const MAX: usize = 10;
/// Missing frames reported as `Lost` (Opus PLC) after the buffer runs dry, before `Empty`.
const DRAIN_PLC: u8 = 2;

#[derive(Default)]
pub struct Jitter {
    /// Sorted by sequence (wrapping), all at or after `next` once playing.
    frames: Vec<(u16, Vec<u8>)>,
    /// Next sequence to play; `None` while (re)buffering up to `TARGET`.
    next: Option<u16>,
    drained: u8,
}

/// `a` comes before `b` in wrapping u16 sequence order.
fn before(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) < 0
}

impl Jitter {
    pub fn push(&mut self, seq: u16, opus: Vec<u8>) {
        if self.next.is_some_and(|n| before(seq, n)) || self.frames.iter().any(|(s, _)| *s == seq) {
            return; // too late, or a duplicate
        }
        let i = self.frames.partition_point(|(s, _)| before(*s, seq));
        self.frames.insert(i, (seq, opus));
        if self.frames.len() > MAX {
            self.frames.remove(0);
            if self.next.is_some() {
                self.next = Some(self.frames[0].0);
            }
        }
    }

    pub fn pop(&mut self) -> Popped {
        let Some(next) = self.next else {
            if self.frames.len() < TARGET {
                return Popped::Empty;
            }
            self.next = Some(self.frames[0].0);
            return self.pop();
        };
        match self.frames.first() {
            Some((seq, _)) if *seq == next => {
                self.next = Some(next.wrapping_add(1));
                self.drained = 0;
                Popped::Frame(self.frames.remove(0).1)
            }
            // the stream jumped (e.g. sender restarted): skip ahead instead of reporting every gap
            Some((seq, _)) if seq.wrapping_sub(next) as usize >= MAX => {
                self.next = Some(*seq);
                self.pop()
            }
            Some(_) => {
                self.next = Some(next.wrapping_add(1));
                Popped::Lost
            }
            None if self.drained < DRAIN_PLC => {
                self.drained += 1;
                self.next = Some(next.wrapping_add(1));
                Popped::Lost
            }
            None => {
                self.next = None;
                self.drained = 0;
                Popped::Empty
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(b: u8) -> Popped {
        Popped::Frame(vec![b])
    }

    #[test]
    fn jitter_reorders() {
        let mut j = Jitter::default();
        for s in [2u16, 1, 3] {
            j.push(s, vec![s as u8]);
        }
        assert_eq!(j.pop(), f(1));
        assert_eq!(j.pop(), f(2));
        assert_eq!(j.pop(), f(3));
    }

    #[test]
    fn jitter_reports_loss_once_then_plc_then_empty() {
        let mut j = Jitter::default();
        for s in [1u16, 2, 3, 5] {
            j.push(s, vec![s as u8]);
        }
        assert_eq!(j.pop(), f(1));
        assert_eq!(j.pop(), f(2));
        assert_eq!(j.pop(), f(3));
        assert_eq!(j.pop(), Popped::Lost);
        j.push(4, vec![4]); // too late: already reported lost, never played
        assert_eq!(j.pop(), f(5));
        // drained: PLC for two frames, then empty (and rebuffer to the target)
        assert_eq!(j.pop(), Popped::Lost);
        assert_eq!(j.pop(), Popped::Lost);
        assert_eq!(j.pop(), Popped::Empty);
        assert_eq!(j.pop(), Popped::Empty);
        j.push(9, vec![9]);
        assert_eq!(j.pop(), Popped::Empty, "waits for 3 frames again");
    }

    #[test]
    fn jitter_waits_for_target_before_first_frame() {
        let mut j = Jitter::default();
        j.push(1, vec![1]);
        j.push(2, vec![2]);
        assert_eq!(j.pop(), Popped::Empty);
        j.push(3, vec![3]);
        assert_eq!(j.pop(), f(1));
    }

    #[test]
    fn jitter_drops_too_old() {
        let mut j = Jitter::default();
        for s in 1..=12u16 {
            j.push(s, vec![s as u8]);
        }
        assert_eq!(j.pop(), f(3), "max 10 frames kept, oldest dropped");
        let mut j = Jitter::default();
        for s in 1..=3u16 {
            j.push(s, vec![s as u8]);
        }
        assert_eq!(j.pop(), f(1));
        j.push(1000, vec![0]); // stream jumped: skip ahead instead of reporting ~1000 losses
        assert_eq!(j.pop(), f(2));
        assert_eq!(j.pop(), f(3));
        assert_eq!(j.pop(), Popped::Frame(vec![0]));
    }

    #[test]
    fn jitter_seq_wrap() {
        let mut j = Jitter::default();
        for s in [65535u16, 0, 65534, 1] {
            j.push(s, vec![s as u8]);
        }
        assert_eq!(j.pop(), f(254));
        assert_eq!(j.pop(), f(255));
        assert_eq!(j.pop(), f(0));
        assert_eq!(j.pop(), f(1));
    }
}
