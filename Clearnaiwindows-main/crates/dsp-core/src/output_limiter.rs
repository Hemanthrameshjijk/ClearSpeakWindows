//! Last stage of the mic chain: a transparent soft limiter so a loud burst (a shout, a cough, the accent or Studio
//! stage adding gain) is compressed instead of hard-clipping. Everything below `KNEE` passes through untouched, so
//! normal speech is not changed at all.

use crate::Stage;

/// Magnitude above which the signal is compressed.
const KNEE: f32 = 0.9;

#[inline]
fn limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let room = 1.0 - KNEE;
        x.signum() * (KNEE + room * ((a - KNEE) / room).tanh())
    }
}

#[derive(Default)]
pub struct OutputLimiter;

impl OutputLimiter {
    pub fn new() -> Self {
        Self
    }
}

impl Stage for OutputLimiter {
    fn name(&self) -> &'static str {
        "output_limiter"
    }

    fn process(&mut self, frame: &mut [f32]) {
        for s in frame.iter_mut() {
            *s = limit(*s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    #[test]
    fn normal_speech_levels_are_untouched() {
        let mut stage = OutputLimiter::new();
        let mut frame: Vec<f32> = (0..FRAME_SAMPLES).map(|i| (i as f32 * 0.05).sin() * 0.85).collect();
        let before = frame.clone();
        stage.process(&mut frame);
        assert_eq!(frame, before);
    }

    #[test]
    fn a_loud_burst_is_compressed_below_full_scale_and_keeps_its_sign() {
        let mut stage = OutputLimiter::new();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        frame[0] = 3.0;
        frame[1] = -3.0;
        frame[2] = 1.0;
        stage.process(&mut frame);
        assert!(frame[0] > 0.9 && frame[0] <= 1.0, "never above full scale, got {}", frame[0]);
        assert!(frame[1] < -0.9 && frame[1] >= -1.0, "got {}", frame[1]);
        assert!(frame[2] > 0.9 && frame[2] < 1.0, "full scale is also softened, got {}", frame[2]);
    }

    #[test]
    fn it_is_monotonic_so_loud_stays_louder_than_quiet() {
        let mut last = -1.0f32;
        for i in 0..=300 {
            let y = limit(i as f32 / 100.0);
            assert!(y >= last);
            last = y;
        }
    }
}
