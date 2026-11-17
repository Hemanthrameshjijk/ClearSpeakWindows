//! Reads the Windows master volume and mute of an output device, for the listening-exposure estimate (the engine
//! only sees digital levels; what reaches the ear also depends on this volume).

/// One reading of an output device's volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputVolume {
    /// Master volume in dB relative to the device's maximum (0 = full, negative = quieter).
    pub db: f32,
    pub muted: bool,
}

/// Volume of the render device `device_id`, or of the default output when `None` (or when that id is gone).
/// Returns `None` when it cannot be read. Cheap enough to call once a second from a non-audio thread.
#[cfg(windows)]
pub fn output_volume(device_id: Option<&str>) -> Option<OutputVolume> {
    use windows::core::{BOOL, HSTRING};
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

    unsafe {
        // Already initialised on this thread is fine (S_FALSE / RPC_E_CHANGED_MODE): either way COM is usable.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let by_id = device_id.and_then(|id| enumerator.GetDevice(&HSTRING::from(id)).ok());
        let device: IMMDevice = match by_id {
            Some(d) => d,
            None => enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?,
        };
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        let db = volume.GetMasterVolumeLevel().ok()?;
        let muted: BOOL = volume.GetMute().ok()?;
        Some(OutputVolume { db, muted: muted.as_bool() })
    }
}

#[cfg(not(windows))]
pub fn output_volume(_device_id: Option<&str>) -> Option<OutputVolume> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    /// Needs a real output device: `cargo test -p audio-io -- --ignored reads_the_default`.
    #[test]
    #[ignore]
    fn reads_the_default_output_volume() {
        let v = super::output_volume(None).expect("default output volume");
        println!("default output: {v:?}");
        assert!(v.db <= 0.5 && v.db > -200.0);
    }
}
