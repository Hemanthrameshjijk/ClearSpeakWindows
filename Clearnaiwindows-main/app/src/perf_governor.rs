//! Low-end PC mode: when this PC cannot keep up, pause the heaviest extras one step at a time instead of letting the
//! audio crackle (or switching everything off). The user's switches and saved settings are never changed; the engine
//! skips the paused stages while `step` is above 0, and brings them back once the PC has kept up for a while.
//!
//! Runs once a second in the engine (`host.rs`), so it works with no window open.
//!
//! The accent is left out on purpose: it manages its own pause and resume exactly as before (pause above RTF 0.97
//! for 5 s, resume below 0.80 for 3 s, see `accent_onnx::pipeline`). Reacting to that pause here as well paused
//! unrelated features and then switched the accent off, which made it flap on a busy PC.

/// Highest step.
pub const MAX_STEP: u8 = 3;

/// Overloaded for this many seconds in a row steps down.
pub const DOWN_AFTER_SECS: u32 = 5;
/// Keeping up for this long steps back up (doubles after each relapse, see `Governor::backoff`).
pub const UP_AFTER_SECS: u32 = 60;
/// Never change step more often than this.
pub const MIN_GAP_SECS: u32 = 30;
/// Longest wait before trying a step back up.
pub const MAX_UP_AFTER_SECS: u32 = 15 * 60;

/// What each step pauses (cumulative).
pub fn step_pauses(step: u8) -> &'static str {
    match step {
        0 => "nothing",
        1 => "Studio and Transcript",
        2 => "Studio, Transcript and caller voice removal",
        _ => "Studio, Transcript and caller voice removal; your voice uses light noise removal",
    }
}

// What the engine skips at each step.
#[inline]
pub fn skip_studio(step: u8) -> bool {
    step >= 1
}
#[inline]
pub fn skip_transcript(step: u8) -> bool {
    step >= 1
}
#[inline]
pub fn skip_speaker_bvc(step: u8) -> bool {
    step >= 2
}
/// Mic background-voice removal replaced by the light RNNoise.
#[inline]
pub fn light_mic_noise(step: u8) -> bool {
    step >= 3
}

/// One second of observations.
#[derive(Clone, Copy, Debug, Default)]
pub struct Signals {
    /// Either audio path is over its time budget (`RealtimeStatus::Overloaded`).
    pub overloaded: bool,
}

#[derive(Debug, Default)]
pub struct Governor {
    step: u8,
    bad: u32,
    good: u32,
    since_change: u32,
    /// Seconds since the last step up, to notice a relapse.
    since_up: Option<u32>,
    backoff: u32,
}

impl Governor {
    pub fn new() -> Self {
        Self { since_change: MIN_GAP_SECS, ..Default::default() }
    }

    pub fn step(&self) -> u8 {
        self.step
    }

    fn up_after(&self) -> u32 {
        (UP_AFTER_SECS << self.backoff.min(4)).min(MAX_UP_AFTER_SECS)
    }

    /// Back to normal at once (the user switched the automatic mode off).
    pub fn reset(&mut self) -> Option<(u8, u8, String)> {
        let old = self.step;
        *self = Self::new();
        (old != 0).then(|| (old, 0, "automatic CPU saving switched off".to_string()))
    }

    /// Call once a second. Returns `(old, new, reason)` when the step changes.
    pub fn tick(&mut self, s: Signals) -> Option<(u8, u8, String)> {
        self.since_change = self.since_change.saturating_add(1);
        if let Some(t) = self.since_up.as_mut() {
            *t += 1;
        }
        self.bad = if s.overloaded { self.bad + 1 } else { 0 };
        self.good = if s.overloaded { 0 } else { self.good + 1 };

        if self.since_change < MIN_GAP_SECS {
            return None;
        }
        let old = self.step;
        if old < MAX_STEP && self.bad >= DOWN_AFTER_SECS {
            // Falling back soon after stepping up: wait longer before the next try.
            if self.since_up.is_some_and(|t| t < 5 * 60) {
                self.backoff += 1;
            }
            let reason = format!("audio over its time budget for {} s", self.bad);
            self.step += 1;
            self.since_change = 0;
            self.bad = 0;
            self.good = 0;
            return Some((old, self.step, reason));
        }
        if old > 0 && self.good >= self.up_after() {
            let reason = format!("kept up for {} s", self.good);
            self.step -= 1;
            self.since_change = 0;
            self.good = 0;
            self.since_up = Some(0);
            return Some((old, self.step, reason));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAD: Signals = Signals { overloaded: true };
    const OK: Signals = Signals { overloaded: false };

    fn run(g: &mut Governor, s: Signals, secs: u32) -> Vec<(u8, u8)> {
        (0..secs).filter_map(|_| g.tick(s).map(|(a, b, _)| (a, b))).collect()
    }

    #[test]
    fn steps_down_after_5_s_overloaded_one_step_per_30_s() {
        let mut g = Governor::new();
        assert_eq!(run(&mut g, BAD, 4), vec![]);
        assert_eq!(run(&mut g, BAD, 1), vec![(0, 1)]);
        assert_eq!(run(&mut g, BAD, 29), vec![], "no change within 30 s");
        assert_eq!(run(&mut g, BAD, 1), vec![(1, 2)]);
        assert_eq!(run(&mut g, BAD, 30), vec![(2, 3)]);
        assert_eq!(run(&mut g, BAD, 300), vec![], "never past the top step");
        assert_eq!(g.step(), MAX_STEP);
    }

    #[test]
    fn steps_never_touch_the_accent() {
        for step in 0..=MAX_STEP {
            assert!(!step_pauses(step).contains("Accent"), "step {step}: the accent manages its own pause");
        }
    }

    #[test]
    fn steps_back_up_after_60_s_of_keeping_up() {
        let mut g = Governor::new();
        run(&mut g, BAD, 5);
        assert_eq!(g.step(), 1);
        assert_eq!(run(&mut g, OK, 59), vec![]);
        assert_eq!(run(&mut g, OK, 1), vec![(1, 0)]);
    }

    #[test]
    fn a_relapse_after_stepping_up_doubles_the_wait() {
        let mut g = Governor::new();
        run(&mut g, BAD, 5);
        run(&mut g, OK, 60); // back to 0
        assert_eq!(g.step(), 0);
        run(&mut g, OK, 30);
        run(&mut g, BAD, 5); // relapse within 5 min
        assert_eq!(g.step(), 1);
        assert_eq!(run(&mut g, OK, 119), vec![], "now waits 120 s");
        assert_eq!(run(&mut g, OK, 1), vec![(1, 0)]);
    }

    #[test]
    fn reset_returns_to_normal() {
        let mut g = Governor::new();
        run(&mut g, BAD, 5);
        assert_eq!(g.reset().map(|(a, b, _)| (a, b)), Some((1, 0)));
        assert_eq!(g.reset(), None);
    }
}
