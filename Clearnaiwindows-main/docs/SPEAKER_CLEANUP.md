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

## Mic-only vs both sides

| | Mic only (fallback) | Both sides |
|---|---|---|
| Build | tag `mic-only-baseline` (4d905b1), installer `...-20Nov-FULL-MICONLY-devcode.exe` | branch `feature/speaker-cleanup` |
| Driver | any ClearSpeak driver (uses `IOCTL_CLEARSPEAK_WRITE` only) | driver with `IOCTL_CLEARSPEAK_READ` (Diver `feature/speaker-read`) |
| Call app microphone | ClearSpeak Microphone | ClearSpeak Microphone |
| Call app speaker | the real headset | **ClearSpeak Speaker** |
| ClearSpeak call audio | off | source "ClearSpeak Speaker (virtual, from the call app)", output = headset |

The new driver keeps the mic contract unchanged (WRITE as before, STATS only grows at the end), so the mic-only
build runs on it too: one driver for both setups, and an agent can go back to mic-only by pointing the call app's
speaker at the headset again.

## Both sides at once: how the CPU is shared

- Two independent real-time threads (mic and caller), each with its own model instances (mic Hush = `weya_nc.dll`,
  caller Hush = `weya_nc_speaker.dll`, separate RNNoise/DeepFilterNet), each BVC behind its own 50 ms watchdog.
- The driver keeps the two directions apart: mic ring (`g_PipeLock`) and speaker ring (`g_SpkLock`).
- When the PC cannot keep up, the caller side gives way first (`perf_governor.rs`): step 1 Studio + Transcript, step 2
  caller voice removal, step 3 (only on a real audio overload) the agent's voice removal becomes light RNNoise.
  Caller RNNoise (~0.1 ms per frame) is never paused.
- The accent counts too: running at RTF 0.90 or above for 5 s steps the governor down to at most step 2, so the
  extras pause before the accent reaches its own pause at 0.97, and never the agent's own voice removal.

Measured (23 Nov, dev PC pinned to 4 logical cores, plugged in; agent side = mic Hush + accent with 2 ORT threads,
both sides paced at 10 ms for 60 s each):

| Caller side | Agent side per frame avg / p99 / max | Caller side avg / p99 / max | Accent RTF (worst) | Frames > 10 ms |
|---|---|---|---|---|
| off | 1.11 / 2.56 / 18.8 ms | - | 0.47 | 1 |
| RNNoise | 1.19 / 2.66 / 4.0 ms | 0.14 / 0.34 / 0.9 ms | 0.48 | 0 |
| RNNoise + Hush Gentle | 1.23 / 2.75 / 4.3 ms | 0.81 / 2.81 / 4.3 ms | 0.46 | 0 |

The caller side adds ~0.1 ms per frame to the agent side and does not move the accent. Agent PCs ran the accent at a
median RTF 0.56 vs 0.44 here (Nov 20 logs), i.e. roughly 1.3x slower cores: still far inside the budget. Their
gaps came from other apps' CPU spikes, which the accent signal in the governor now answers by pausing the extras.

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
