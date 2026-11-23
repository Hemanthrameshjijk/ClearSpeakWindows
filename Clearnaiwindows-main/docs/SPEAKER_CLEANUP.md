# Caller-side cleanup (noise and background voices on the call audio)

What the agent hears from the customer can be cleaned the same way the agent's own microphone is: noise removal
(RNNoise) and, optionally, background voice removal (Hush or DeepFilterNet). This page covers how the call audio gets
into ClearSpeak, what runs on it, the defaults and why, and what is still open.

## Path

```
call app (eyeBeam, Teams, ...) plays into
   either  "ClearSpeak Speaker"  (ClearSpeak driver; read with IOCTL_CLEARSPEAK_READ, app/src/driver_speaker.rs)
   or      a second virtual cable's input (VB-CABLE style; its output captured with WASAPI)
 -> speaker ring (2 s) -> thread clearspeak-speaker-pipeline (engine.rs, spawn_speaker_pipeline)
      RNNoise (caller noise)  -> BVC (Off / Hush / DeepFilterNet, 50 ms watchdog)  -> Studio
      -> shock guard + limiter -> exposure meter -> clock-drift compensation
 -> real headset (HardwareRender)            + a copy to the mic path's echo removal (echo reference)
```

- The source is picked in Devices > Speaker output. The driver source is listed as
  "ClearSpeak Speaker (virtual, from the call app)" only when the installed driver answers the read call
  (`driver_speaker::available`); an older driver keeps working for the microphone and simply does not offer it.
- If no real output is chosen, the Windows default is used, except when that default is a virtual device (for example
  "ClearSpeak Speaker" itself): then the first real output is used, so the cleaned call can never loop back into its
  own source (`engine::real_speaker_output`).
- Clock drift: the call source and the headset run on different clocks. The speaker path now uses the same
  `DriftCompensator` as the mic path (at most 1 % stretch, bit-exact while in step), so a long call neither runs dry
  nor builds up delay. The per-minute `[health] caller audio` line reports the ratio range and restarts.

## Defaults and why (Phase 0, 23 Nov 2026)

Measured on 4 real eyeBeam call recordings (deepak, 20 Nov; ~12 min, 69 s of customer speech; mono mix split by
bandwidth: narrowband = customer, wideband = agent), each option run frame by frame as the speaker pipeline does:

| Option | Customer speech | Customer background | CPU per 10 ms frame (12-core dev PC) |
|---|---|---|---|
| RNNoise | 0 dB | 0 dB | 0.06 ms |
| Hush Gentle (12 dB) | -5.2 dB | -2.5 dB | 0.3 ms |
| Hush Maximum (30 dB) | -8.2 dB | -3.4 dB | 0.4 ms |
| DeepFilterNet | -4.4 dB | -4.8 dB | 0.3 ms |
| RNNoise + Hush 30 dB (old default) | -8.5 dB | -3.4 dB | 0.5 ms |

Hush (built for the agent's own mic) removed more of the customer's voice than of their background, and these
calls had little customer-side background to remove. So for new installs:

- caller BVC model: **Off** (caller cleanup = RNNoise only) - `settings::default_speaker_bvc_backend`;
- caller strength (only used when Hush is picked): **Gentle** - new setting `speaker_nr_strength`, picker on the
  Call Audio tab, preset index 7 over the pipe (`ipc::PRESET_COUNT` = 8).

Existing settings files keep whatever the agent chose.

## Agent setup with the driver (no second cable)

1. Install the ClearSpeak driver build that has `IOCTL_CLEARSPEAK_READ` (ClearnaiWindows-Diver, branch
   `feature/speaker-read`).
2. In the call app, set the speaker/output device to **ClearSpeak Speaker**.
3. In ClearSpeak > Devices > Speaker output: source "ClearSpeak Speaker (virtual, from the call app)", output = the
   real headset. Switch call audio on; Noise removal on in the Call Audio tab.

If ClearSpeak is not running, nothing plays the call on the headset: the agent hears nothing. Keep ClearSpeak set to
start with Windows on these PCs.

## Still open

- **Driver on a real PC / VM**: the read path must be installed and tested in the Hyper-V VM (`tools/vm`, as for the
  microphone path: tone in -> read back, soak, Driver Verifier) and then signed for production (`docs/SIGNING.md` in
  the driver project). Not done yet.
- **Noisy customer calls**: Phase 0 had none. Before offering caller BVC widely, run `app/examples` style evaluation
  (the Phase 0 tool) on calls with traffic/TV/other voices behind the customer.
- **Echo removal tuning** with the cleaned call as reference (delay and double-talk), needs speaker-headset echo
  recordings.
- **CPU on 4-core agent PCs**: caller RNNoise is ~0.1 ms per frame; Hush on the caller side is a second DLL session.
  The low-end PC governor already drops caller BVC at step 2.
