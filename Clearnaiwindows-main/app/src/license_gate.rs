//! Turns the license state into what the engine may run. Lives in the engine (the always-on process), so closing the
//! window or editing the window's toggles does not get around it.
//!
//! Licensed: the features in the token. Not signed in, a license that no longer works here, or a clock turned back:
//! the three processing features are off and the microphone passes through untouched (calls keep working).

use crate::settings::Settings;
use license::Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowed {
    pub nc: bool,
    pub bvc: bool,
    pub accent: bool,
}

impl Allowed {
    pub const ALL: Allowed = Allowed { nc: true, bvc: true, accent: true };
    pub const NONE: Allowed = Allowed { nc: false, bvc: false, accent: false };
}

pub fn allowed(status: &Status) -> Allowed {
    match status {
        Status::Licensed(p) => Allowed { nc: p.has_feature("nc"), bvc: p.has_feature("bvc"), accent: p.has_feature("accent") },
        Status::Ended | Status::SignIn => Allowed::NONE,
    }
}

/// The state for these settings on this PC at `now`. A clock more than a day behind the latest time seen counts as no license.
pub fn status_for(settings: &Settings, devices: &[String], now: u64, registry_mark: u64) -> Status {
    let seen = settings.last_seen_at.max(registry_mark);
    if crate::clock_guard::rolled_back(seen, now) {
        return Status::Ended;
    }
    let token = crate::dpapi::unprotect(&settings.license_key);
    if let Some(s) = crate::dev_code::status(&token, now) {
        return s;
    }
    license::status(&token, &crate::license_key::PUBLIC_KEY, devices, now)
}

/// A feature the license allows again (renewed, clock fixed, back on the right PC) comes back as the user had it:
/// the saved settings were never changed by the gate, so they still say what the user wants.
pub fn restore(h: &crate::shared::EngineHandles, before: Allowed, now: Allowed, s: &crate::settings::Settings) {
    use tse_engine::{BvcBackend, SpeakerBvcBackend};
    if now.nc && !before.nc {
        h.mic_noise.set(s.mic_noise_on);
        h.speaker_noise.set(s.speaker_noise_on);
    }
    if now.bvc && !before.bvc {
        let mic_ok = match BvcBackend::from_index(s.mic_bvc_backend) {
            BvcBackend::Off => false,
            BvcBackend::Hush => h.mic_bvc_available,
            BvcBackend::DeepFilter => h.mic_deep_filter_available,
            BvcBackend::HushBpoV1 => h.mic_bpo_v1_available,
        };
        h.mic_bvc.set(s.mic_bvc_on && mic_ok);
        let spk_ok = match SpeakerBvcBackend::from_index(s.speaker_bvc_backend) {
            SpeakerBvcBackend::Off => false,
            SpeakerBvcBackend::Hush => h.speaker_bvc_available,
            SpeakerBvcBackend::DeepFilter => h.speaker_deep_filter_available,
        };
        h.speaker_bvc.set(s.speaker_bvc_on && spk_ok);
    }
    if now.accent && !before.accent {
        h.accent.set_voice(s.accent_effective_voice());
    }
}

/// Switches off whatever the license does not allow. Only ever turns things off.
pub fn enforce(h: &crate::shared::EngineHandles, a: Allowed) {
    if !a.nc {
        h.mic_noise.set(false);
        h.speaker_noise.set(false);
    }
    if !a.bvc {
        h.mic_bvc.set(false);
        h.speaker_bvc.set(false);
    }
    if !a.accent {
        h.accent.set_voice(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_with(feat: &[&str]) -> Status {
        Status::Licensed(license::Payload {
            v: 1,
            lid: "l".into(),
            org: "o".into(),
            uid: "u".into(),
            email: String::new(), orgname: String::new(),
            name: "A".into(),
            dev: vec![],
            min: 2,
            feat: feat.iter().map(|s| s.to_string()).collect(),
            iat: 0,
            exp: u64::MAX,
        })
    }

    #[test]
    fn without_a_working_license_nothing_is_allowed() {
        assert_eq!(allowed(&Status::Ended), Allowed::NONE);
        assert_eq!(allowed(&Status::SignIn), Allowed::NONE, "until someone signs in, the mic passes through untouched");
    }

    #[test]
    fn a_license_allows_exactly_its_features() {
        assert_eq!(allowed(&token_with(&["nc", "accent"])), Allowed { nc: true, bvc: false, accent: true });
        assert_eq!(allowed(&token_with(&[])), Allowed::NONE);
        assert_eq!(allowed(&token_with(&["nc", "bvc", "accent"])), Allowed::ALL);
    }

    #[test]
    fn a_turned_back_clock_means_no_license() {
        let mut s = Settings::default();
        s.last_seen_at = 10_000_000;
        assert_eq!(status_for(&s, &[], 10_000_000 - 2 * 86_400, 0), Status::Ended);
        assert_eq!(status_for(&s, &[], 10_000_000 + 60, 0), Status::SignIn, "a right clock and no license: sign in");
        let reg_only = Settings::default();
        assert_eq!(status_for(&reg_only, &[], 10_000_000 - 2 * 86_400, 10_000_000), Status::Ended, "the registry mark counts too");
    }

    #[test]
    fn features_come_back_as_the_user_had_them_when_the_license_allows_again() {
        let mut s = crate::settings::Settings::default();
        s.mic_noise_on = true;
        s.mic_bvc_on = true;
        s.accent_on = true;
        let h = crate::shared::EngineHandles::new_from_settings(
            &s, true, None, true, None, true, None, vec![], true, None, true, None,
        );
        let all = Allowed { nc: true, bvc: true, accent: true };
        enforce(&h, Allowed::NONE);
        assert!(!h.mic_noise.is_on() && !h.mic_bvc.is_on());
        restore(&h, Allowed::NONE, all, &s);
        assert!(h.mic_noise.is_on() && h.mic_bvc.is_on(), "back on after renewal");
        assert_eq!(h.accent.voice(), s.accent_effective_voice());
        // Still allowed: nothing is touched (a feature the user switched off stays off).
        h.mic_noise.set(false);
        restore(&h, all, all, &s);
        assert!(!h.mic_noise.is_on());
    }
}
