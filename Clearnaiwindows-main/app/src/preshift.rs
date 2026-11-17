//! Pre-shift check: scores the 20 s "Test my voice" recording so an agent (and later a supervisor) knows before the
//! first call whether the headset is good enough: speech level, background noise, clipping, Bluetooth dropouts and
//! whether this PC can run the accent in real time. Pure analysis here; `voice_test` records and shows it.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    #[default]
    Pass,
    Warn,
    Fail,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "Pass",
            Verdict::Warn => "Check",
            Verdict::Fail => "Fix",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Check {
    pub key: String,
    pub label: String,
    /// The measured value, readable ("-22 dB", "0.0%").
    pub value: String,
    pub verdict: Verdict,
    /// What to do about it (empty on a pass).
    pub tip: String,
}

/// What the capture side reported during the recording.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptureInfo {
    /// Discontinuities the device reported (lost packets, mostly Bluetooth).
    pub gaps: u64,
    /// Packets the device marked silent.
    pub silent_packets: u64,
    /// Bluetooth headset in call (Hands-Free) mode.
    pub hands_free: bool,
    pub device: String,
    /// Mic boost (dB) the live mic path adds before everything else; the voice level is judged after it.
    pub boost_db: f64,
}

const WIN: usize = 960; // 20 ms at 48 kHz

fn db(x: f64) -> f64 {
    if x <= 1e-12 {
        -120.0
    } else {
        10.0 * x.log10()
    }
}

/// Mean-square energy of each 20 ms window.
fn window_energies(x: &[f32]) -> Vec<f64> {
    x.chunks(WIN)
        .filter(|c| c.len() == WIN)
        .map(|c| c.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / WIN as f64)
        .collect()
}

/// Speech level: energy mean of the loudest 30% of windows, in dBFS.
pub fn speech_level_db(x: &[f32]) -> f64 {
    let mut e = window_energies(x);
    if e.is_empty() {
        return -120.0;
    }
    e.sort_by(|a, b| b.total_cmp(a));
    let n = (e.len() * 3 / 10).max(1);
    db(e[..n].iter().sum::<f64>() / n as f64)
}

/// Background noise: the 10th-percentile window, in dBFS.
pub fn noise_floor_db(x: &[f32]) -> f64 {
    let mut e = window_energies(x);
    if e.is_empty() {
        return -120.0;
    }
    e.sort_by(|a, b| a.total_cmp(b));
    db(e[e.len() / 10])
}

/// Share of samples at or above full scale, 0..1.
pub fn clipped_fraction(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    x.iter().filter(|s| s.abs() >= 0.99).count() as f64 / x.len() as f64
}

fn check(key: &str, label: &str, value: String, verdict: Verdict, tip: &str) -> Check {
    Check {
        key: key.into(),
        label: label.into(),
        value,
        verdict,
        tip: if verdict == Verdict::Pass { String::new() } else { tip.into() },
    }
}

/// The checks that only need the raw recording and what the capture reported.
pub fn analyse(rec: &[f32], sample_rate: usize, info: &CaptureInfo) -> Vec<Check> {
    let mut out = Vec::new();
    let secs = rec.len() as f64 / sample_rate as f64;

    // Judged as the live path hears it: after Mic boost (earbud mics are often quiet and rely on it).
    let speech = speech_level_db(rec) + info.boost_db;
    let quiet_tip = if info.boost_db > 0.0 {
        "Your voice is quiet even with Mic boost. Move the microphone closer to your mouth or turn Mic boost up."
    } else {
        "Your voice is quiet. Move the microphone closer to your mouth or turn on Mic boost."
    };
    let (v, tip) = if speech < -40.0 {
        (Verdict::Fail, "We could not hear you. Check that the right microphone is chosen and not muted, then speak normally.")
    } else if speech < -30.0 {
        (Verdict::Warn, quiet_tip)
    } else if speech > -6.0 {
        (Verdict::Fail, "Your voice is far too loud. Move the microphone away from your mouth or lower the mic level in Windows.")
    } else if speech > -12.0 {
        (Verdict::Warn, "Your voice is very loud. Move the microphone a little away from your mouth.")
    } else {
        (Verdict::Pass, "")
    };
    out.push(check("speech", "Voice level", format!("{speech:.0} dB"), v, tip));

    // Judged against the voice, not on its own: mic gain differs a lot between headsets, laptops and USB mics.
    let floor = noise_floor_db(rec) + info.boost_db;
    let snr = speech - floor;
    let (v, tip) = if snr < 15.0 {
        (Verdict::Fail, "The background is almost as loud as your voice. Move away from fans, speakers and other people, or use a headset mic close to your mouth.")
    } else if snr < 25.0 {
        (Verdict::Warn, "Some background noise. Noise removal will handle it, but a quieter spot or a headset mic sounds better.")
    } else {
        (Verdict::Pass, "")
    };
    out.push(check("noise", "Background noise", format!("{snr:.0} dB below your voice"), v, tip));

    let clip = clipped_fraction(rec);
    let (v, tip) = if clip >= 0.01 {
        (Verdict::Fail, "Your voice is distorting. Lower the mic level in Windows or turn off Mic boost.")
    } else if clip >= 0.001 {
        (Verdict::Warn, "A few loud moments distorted. Keep the microphone a finger's width from your mouth.")
    } else {
        (Verdict::Pass, "")
    };
    out.push(check("clip", "Distortion", format!("{:.1}%", clip * 100.0), v, tip));

    let per10 = if secs > 0.0 { info.gaps as f64 / secs * 10.0 } else { 0.0 };
    let (v, tip) = if per10 >= 6.0 {
        (Verdict::Fail, "The headset keeps losing sound. Charge it, keep the phone and PC close, or use a wired/USB headset.")
    } else if per10 >= 2.0 {
        (Verdict::Warn, "The headset dropped some sound. Move closer to the PC or charge the headset.")
    } else {
        (Verdict::Pass, "")
    };
    out.push(check("dropouts", "Dropouts", format!("{} in {:.0} s", info.gaps, secs), v, tip));

    if info.hands_free {
        out.push(check(
            "bluetooth",
            "Headset mode",
            "Bluetooth call mode".into(),
            Verdict::Warn,
            "Bluetooth call mode lowers the quality of your voice and of what you hear. A USB or wired headset sounds better.",
        ));
    }
    out
}

/// How fast the accent ran on this PC: `work_secs` of processing for `audio_secs` of audio (lower is faster).
pub fn speed_check(work_secs: f64, audio_secs: f64) -> Check {
    let rtf = if audio_secs > 0.0 { work_secs / audio_secs } else { 0.0 };
    let (v, tip) = if rtf >= 1.0 {
        (Verdict::Fail, "This PC is too slow for the accent; it will pause during calls. Close other apps, or ask IT for a faster PC.")
    } else if rtf >= 0.8 {
        (Verdict::Warn, "This PC is only just fast enough for the accent. Close other apps before calls.")
    } else {
        (Verdict::Pass, "")
    };
    check("speed", "PC speed for accent", format!("{:.0}% busy", rtf * 100.0), v, tip)
}

/// Worst verdict of all checks.
pub fn overall(checks: &[Check]) -> Verdict {
    checks.iter().map(|c| c.verdict).max().unwrap_or_default()
}

pub fn overall_line(v: Verdict) -> &'static str {
    match v {
        Verdict::Pass => "Ready for calls: your headset and PC passed every check.",
        Verdict::Warn => "Ready, with something to look at below.",
        Verdict::Fail => "Not ready: fix the items below before your first call.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: usize = 48_000;

    fn speechy(amp: f32, secs: f32) -> Vec<f32> {
        // Bursts of a 220 Hz tone with quiet gaps and a little noise, like speech.
        let mut seed = 1u32;
        (0..(secs * SR as f32) as usize)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5) * 0.0005;
                let on = (i / 9600) % 2 == 0;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * i as f32 / SR as f32).sin();
                if on { amp * tone + noise } else { noise }
            })
            .collect()
    }

    fn verdict(cs: &[Check], key: &str) -> Verdict {
        cs.iter().find(|c| c.key == key).unwrap().verdict
    }

    #[test]
    fn a_good_recording_passes() {
        let rec = speechy(0.14, 10.0); // about -20 dBFS while talking
        let cs = analyse(&rec, SR, &CaptureInfo::default());
        assert_eq!(overall(&cs), Verdict::Pass, "{cs:#?}");
    }

    #[test]
    fn silence_fails_on_voice_level() {
        let cs = analyse(&vec![0.0; SR * 5], SR, &CaptureInfo::default());
        assert_eq!(verdict(&cs, "speech"), Verdict::Fail);
        assert_eq!(overall(&cs), Verdict::Fail);
    }

    #[test]
    fn a_clipped_recording_fails_on_distortion() {
        let rec: Vec<f32> = speechy(3.0, 5.0).into_iter().map(|s| s.clamp(-1.0, 1.0)).collect();
        assert_eq!(verdict(&analyse(&rec, SR, &CaptureInfo::default()), "clip"), Verdict::Fail);
    }

    #[test]
    fn steady_noise_fails_on_background_noise() {
        let rec: Vec<f32> = (0..SR * 5).map(|i| 0.05 * (i as f32 * 0.37).sin()).collect();
        assert_eq!(verdict(&analyse(&rec, SR, &CaptureInfo::default()), "noise"), Verdict::Fail);
    }

    #[test]
    fn bluetooth_dropouts_and_call_mode_are_reported() {
        let rec = speechy(0.14, 20.0);
        let info = CaptureInfo { gaps: 20, hands_free: true, ..Default::default() };
        let cs = analyse(&rec, SR, &info);
        assert_eq!(verdict(&cs, "dropouts"), Verdict::Fail);
        assert_eq!(verdict(&cs, "bluetooth"), Verdict::Warn);
    }

    #[test]
    fn mic_boost_counts_toward_the_voice_level() {
        let rec = speechy(0.012, 10.0); // about -41 dBFS raw: a quiet earbud
        assert_eq!(verdict(&analyse(&rec, SR, &CaptureInfo::default()), "speech"), Verdict::Fail);
        let boosted = CaptureInfo { boost_db: 18.0, ..Default::default() };
        let cs = analyse(&rec, SR, &boosted);
        assert_eq!(verdict(&cs, "speech"), Verdict::Pass, "{cs:#?}");
        assert_eq!(verdict(&cs, "noise"), verdict(&analyse(&rec, SR, &CaptureInfo::default()), "noise"));
    }

    #[test]
    fn accent_speed_thresholds() {
        assert_eq!(speed_check(10.0, 20.0).verdict, Verdict::Pass);
        assert_eq!(speed_check(17.0, 20.0).verdict, Verdict::Warn);
        assert_eq!(speed_check(25.0, 20.0).verdict, Verdict::Fail);
        assert!(speed_check(10.0, 20.0).tip.is_empty());
    }
}
