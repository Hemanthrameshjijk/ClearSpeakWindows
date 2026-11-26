//! Cuts a continuous 16 kHz stream into utterances with a simple adaptive-energy speech detector.
//!
//! A segment starts after 60 ms of speech (with 300 ms of look-back kept), ends after 700 ms of silence or at 10 s,
//! and while it is open (up to 6 s) a *partial* snapshot is offered every 1.5 s so the GUI can show text as you speak.
//! Each partial re-reads the whole open segment, so during long speech without pauses the cost grew with every
//! snapshot and took CPU from accent conversion (heard as skips); partials stop at 6 s and segments end at 10 s.

const FRAME: usize = 320; // 20 ms
const PREROLL_FRAMES: usize = 15; // 300 ms
const START_FRAMES: usize = 3; // 60 ms above threshold
const END_SILENCE_FRAMES: usize = 35; // 700 ms
const KEEP_TAIL_FRAMES: usize = 10; // 200 ms of the trailing silence stays in the segment
const MIN_SPEECH_FRAMES: usize = 20; // shorter than 400 ms of speech: ignored (clicks, coughs)
const MAX_SEGMENT: usize = 10 * 16_000;
const PARTIAL_MAX: usize = 6 * 16_000; // no partial snapshots of a longer open segment (the final comes soon)
const PARTIAL_EVERY: usize = 24_000; // 1.5 s
/// Lowest speech threshold (RMS, about -56 dBFS). It used to be 0.006 (-44 dBFS): a quiet laptop mic (peaks near
/// -28 dBFS, speech RMS near -50) never crossed it, so the transcript stayed empty although the model reads such audio
/// fine (it normalises its features). The tap sits after noise removal, so the noise floor there is very low.
const MIN_THRESHOLD: f32 = 0.0015;
/// A segment that hits `MAX_SEGMENT` is cut at the quietest 20 ms in its last 2 s (between words), not mid-word;
/// the rest starts the next segment.
const CUT_SEARCH_FRAMES: usize = 100;
/// Starting (and lowest) noise estimate.
const NOISE_MIN: f32 = 0.0005;

#[derive(Debug, PartialEq)]
pub enum Event {
    /// Snapshot of the still-open segment.
    Partial(Vec<f32>),
    /// A finished segment.
    Final(Vec<f32>),
}

pub struct Segmenter {
    pending: Vec<f32>,
    preroll: std::collections::VecDeque<Vec<f32>>,
    buf: Vec<f32>,
    /// RMS of each 20 ms frame in `buf` (same order), to find a quiet place to cut a too-long segment.
    levels: Vec<f32>,
    in_speech: bool,
    above: usize,
    silence: usize,
    speech_frames: usize,
    noise: f32,
    last_partial_len: usize,
    /// Samples taken in as whole frames so far, and where (in those samples) the last `Final` ended.
    fed: usize,
    last_end: usize,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            preroll: std::collections::VecDeque::new(),
            buf: Vec::new(),
            levels: Vec::new(),
            in_speech: false,
            above: 0,
            silence: 0,
            speech_frames: 0,
            noise: NOISE_MIN,
            last_partial_len: 0,
            fed: 0,
            last_end: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Where the last [`Event::Final`] ended, in samples since the start (or the last reset). Its start is this minus
    /// its length. Push at most one frame at a time when you need this for every segment.
    pub fn last_final_end(&self) -> usize {
        self.last_end
    }

    /// Samples taken in as whole 20 ms frames since the start (or the last reset): with [`Self::last_final_end`],
    /// how far back the last finished segment ended.
    pub fn fed(&self) -> usize {
        self.fed
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    pub fn push(&mut self, x16: &[f32]) -> Vec<Event> {
        let mut events = Vec::new();
        self.pending.extend_from_slice(x16);
        let mut off = 0;
        while self.pending.len() - off >= FRAME {
            let frame = self.pending[off..off + FRAME].to_vec();
            off += FRAME;
            self.frame(frame, &mut events);
        }
        self.pending.drain(..off);
        events
    }

    fn finish(&mut self, events: &mut Vec<Event>) {
        let drop_tail = self.silence.saturating_sub(KEEP_TAIL_FRAMES) * FRAME;
        let keep = self.buf.len().saturating_sub(drop_tail);
        self.buf.truncate(keep);
        self.last_end = self.fed - drop_tail;
        if self.speech_frames >= MIN_SPEECH_FRAMES {
            events.push(Event::Final(std::mem::take(&mut self.buf)));
        }
        self.buf.clear();
        self.levels.clear();
        self.in_speech = false;
        self.silence = 0;
        self.above = 0;
        self.speech_frames = 0;
        self.last_partial_len = 0;
    }

    /// Too long without a pause: emit up to the quietest frame of the last 2 s and keep the rest open as the start
    /// of the next segment, so a word is not split across two lines.
    fn cut_long(&mut self, events: &mut Vec<Event>) {
        let frames = self.levels.len();
        let from = frames.saturating_sub(CUT_SEARCH_FRAMES).max(1);
        let mut cut = frames;
        for i in from..frames {
            // `<=`: of equally quiet frames take the latest, so steady sound still gives near-10 s lines.
            if cut == frames || self.levels[i] <= self.levels[cut] {
                cut = i;
            }
        }
        let rest = self.buf.split_off(cut * FRAME);
        let rest_levels = self.levels.split_off(cut);
        self.last_end = self.fed - rest.len();
        events.push(Event::Final(std::mem::replace(&mut self.buf, rest)));
        self.levels = rest_levels;
        // The remainder is still speech in progress.
        self.speech_frames = self.levels.len();
        self.silence = 0;
        self.last_partial_len = 0;
    }

    fn frame(&mut self, frame: Vec<f32>, events: &mut Vec<Event>) {
        self.fed += frame.len();
        let rms = frame_rms(&frame);
        let thr = (self.noise * 3.0).max(MIN_THRESHOLD);
        let loud = rms > thr;
        // Noise floor: falls fast, rises slowly (~20 s), on every frame. Quiet speech must not be learned as "noise"
        // (the threshold would climb above the voice and sentences were missed), but steady noise (fan, hum with noise
        // removal off) must still be learned even while it counts as "speech", or it would never end a segment.
        // Gaps between words pull it back down quickly.
        let r = rms.clamp(NOISE_MIN, 0.05);
        let k = if r < self.noise { 0.2 } else { 0.001 };
        self.noise += k * (r - self.noise);
        if !self.in_speech {
            self.above = if loud { self.above + 1 } else { 0 };
            if self.above >= START_FRAMES {
                self.in_speech = true;
                self.buf.clear();
                self.levels.clear();
                for f in self.preroll.drain(..) {
                    self.levels.push(frame_rms(&f));
                    self.buf.extend_from_slice(&f);
                }
                self.levels.push(rms);
                self.buf.extend_from_slice(&frame);
                self.speech_frames = self.above;
                self.silence = 0;
                self.last_partial_len = 0;
            } else {
                self.preroll.push_back(frame);
                if self.preroll.len() > PREROLL_FRAMES {
                    self.preroll.pop_front();
                }
            }
            return;
        }
        self.buf.extend_from_slice(&frame);
        self.levels.push(rms);
        if loud {
            self.silence = 0;
            self.speech_frames += 1;
        } else {
            self.silence += 1;
        }
        if self.silence >= END_SILENCE_FRAMES {
            self.finish(events);
        } else if self.buf.len() >= MAX_SEGMENT {
            self.cut_long(events);
        } else if self.speech_frames >= MIN_SPEECH_FRAMES
            && self.buf.len() <= PARTIAL_MAX
            && self.buf.len() >= self.last_partial_len + PARTIAL_EVERY
        {
            self.last_partial_len = self.buf.len();
            events.push(Event::Partial(self.buf.clone()));
        }
    }
}

fn frame_rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|v| v * v).sum::<f32>() / frame.len().max(1) as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_microphone_is_still_transcribed() {
        // Speech RMS near -50 dBFS (a laptop mic peaking around -28 dBFS) with a near-silent denoised floor.
        let mut s = Segmenter::new();
        let mut ev = Vec::new();
        ev.extend(s.push(&vec![0.00005; 16_000]));
        ev.extend(s.push(&tone(2.0, 0.0045)));
        ev.extend(s.push(&vec![0.00005; 16_000]));
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Final(_))).count(), 1, "quiet speech must make a line");
    }

    #[test]
    fn steady_noise_stops_making_lines() {
        // A minute of steady fan noise at -40 dBFS (above the quiet-speech threshold, noise removal off).
        let mut s = Segmenter::new();
        let noise: Vec<f32> = (0..60 * 16_000).map(|i| 0.01 * (((i * 7919) % 1000) as f32 / 500.0 - 1.0) * 1.7).collect();
        let ev = s.push(&noise);
        let finals = ev.iter().filter(|e| matches!(e, Event::Final(_))).count();
        assert!(finals <= 3, "steady noise kept producing lines: {finals}");
        assert!(!s.in_speech(), "and is not 'speech' after a minute");
    }

    #[test]
    fn a_long_segment_is_cut_in_a_pause_not_mid_word() {
        // 9 s of speech, a 200 ms dip (shorter than the 700 ms that ends a segment), then 3 s more.
        let mut s = Segmenter::new();
        let mut ev = Vec::new();
        ev.extend(s.push(&tone(9.0, 0.2)));
        ev.extend(s.push(&vec![0.0; 3_200]));
        ev.extend(s.push(&tone(3.0, 0.2)));
        ev.extend(s.push(&vec![0.0; 16_000]));
        let finals: Vec<usize> = ev.iter().filter_map(|e| if let Event::Final(a) = e { Some(a.len()) } else { None }).collect();
        assert_eq!(finals.len(), 2, "{finals:?}");
        // The cut lands inside the dip (9.0..9.2 s of speech + 300 ms look-back of the start).
        let cut_s = finals[0] as f32 / 16_000.0;
        assert!((9.0..=9.5).contains(&cut_s), "cut at {cut_s} s");
        let total: usize = finals.iter().sum();
        assert!(total as f32 / 16_000.0 > 12.0, "no audio lost at the cut: {finals:?}");
    }

    fn tone(secs: f32, amp: f32) -> Vec<f32> {
        (0..(secs * 16_000.0) as usize).map(|i| amp * (i as f32 * 0.3).sin()).collect()
    }

    #[test]
    fn splits_two_utterances_and_ignores_a_click() {
        let mut s = Segmenter::new();
        let mut ev = Vec::new();
        ev.extend(s.push(&vec![0.0; 16_000])); // 1 s silence
        ev.extend(s.push(&tone(2.0, 0.2))); // utterance 1
        ev.extend(s.push(&vec![0.0; 16_000])); // 1 s gap
        ev.extend(s.push(&tone(0.1, 0.2))); // click: too short
        ev.extend(s.push(&vec![0.0; 16_000]));
        ev.extend(s.push(&tone(1.0, 0.2))); // utterance 2
        ev.extend(s.push(&vec![0.0; 16_000]));
        let finals: Vec<usize> = ev.iter().filter_map(|e| if let Event::Final(a) = e { Some(a.len()) } else { None }).collect();
        assert_eq!(finals.len(), 2, "events: {} finals {:?}", ev.len(), finals);
        // 2 s of speech + 300 ms look-back + 200 ms tail
        assert!((finals[0] as f32 / 16_000.0 - 2.5).abs() < 0.1, "{finals:?}");
        assert!(ev.iter().any(|e| matches!(e, Event::Partial(_))), "a 2 s utterance must offer a partial");
    }

    #[test]
    fn long_speech_is_cut_at_fifteen_seconds() {
        let mut s = Segmenter::new();
        let ev = s.push(&tone(16.0, 0.2));
        // Cut somewhere in the last 2 s before the 10 s limit, never longer.
        assert!(ev.iter().any(|e| matches!(e, Event::Final(a) if a.len() >= MAX_SEGMENT - CUT_SEARCH_FRAMES * FRAME)));
        assert!(ev.iter().all(|e| !matches!(e, Event::Final(a) if a.len() > MAX_SEGMENT)));
        assert!(
            ev.iter().all(|e| !matches!(e, Event::Partial(a) if a.len() > PARTIAL_MAX)),
            "long speech: no partial re-reads more than 6 s"
        );
    }
}
