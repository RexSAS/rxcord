//! Voice activity: open on speech, hold 200 ms after it, then close once.

#[derive(Debug, PartialEq)]
pub enum GateOut {
    Send,
    /// Speech just ended: emit 5 Opus silence frames.
    Stop,
    Idle,
}

pub struct Gate {
    /// Speech probability that opens the gate.
    pub threshold: f32,
    /// Frames still sent after the last speech frame.
    pub hangover_frames: u32,
    left: u32,
    open: bool,
}

impl Gate {
    pub fn new(threshold: f32, hangover_frames: u32) -> Gate {
        Gate { threshold, hangover_frames, left: 0, open: false }
    }

    /// One frame's speech probability in, what to do with the frame out.
    pub fn update(&mut self, speech_prob: f32) -> GateOut {
        if speech_prob >= self.threshold {
            self.left = self.hangover_frames;
            self.open = true;
            GateOut::Send
        } else if self.left > 0 {
            self.left -= 1;
            GateOut::Send
        } else if self.open {
            self.open = false;
            GateOut::Stop
        } else {
            GateOut::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_hangover() {
        let mut g = Gate::new(0.6, 10);
        assert_eq!(g.update(0.0), GateOut::Idle);
        assert_eq!(g.update(0.9), GateOut::Send);
        for i in 0..10 {
            assert_eq!(g.update(0.0), GateOut::Send, "hangover frame {i}");
        }
        assert_eq!(g.update(0.0), GateOut::Stop);
        assert_eq!(g.update(0.0), GateOut::Idle);
        assert_eq!(g.update(0.0), GateOut::Idle);
    }

    #[test]
    fn gate_threshold() {
        let mut g = Gate::new(0.6, 0);
        assert_eq!(g.update(0.59), GateOut::Idle);
        assert_eq!(g.update(0.6), GateOut::Send, "at the threshold counts");
        assert_eq!(g.update(0.59), GateOut::Stop);
        g.threshold = 0.1;
        assert_eq!(g.update(0.2), GateOut::Send);
        assert_eq!(g.update(0.05), GateOut::Stop);
    }
}
