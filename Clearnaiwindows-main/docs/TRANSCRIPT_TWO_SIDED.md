# Two-sided transcript (Agent + Caller) — design

Status: **proposal, not built.** Jira: KRIS-58 (decide whether transcript is in the product), KRIS-59 (caller audio and
Agent/Caller labels), KRIS-60 (model in the installer, privacy), KRIS-51 (caller audio through the ClearNAI Speaker
endpoint).

## 1. Today

| Part | State |
|---|---|
| Caller-side cleanup | Built. `engine::spawn_speaker_pipeline`: RNNoise → speaker BVC (own Hush/DeepFilterNet DLL instance) → studio → real headset. The cleaned call audio also feeds the mic's echo removal (`echo_ref`). Off by default (`call_audio_on`). Needs the call app's speaker set to a virtual device (today a second cable; KRIS-51 moves it to the ClearNAI Speaker endpoint). |
| Transcript | Mic only. `asr_onnx::pipeline`: the mic thread copies frames into a ring (`TranscriptStage::process`), one worker downsamples 48 → 16 kHz, cuts utterances (`Segmenter`), runs the FastConformer model (`AsrModel`) and publishes lines (`publish_line`). One file per app session in `%LOCALAPPDATA%\ClearNAI\transcripts`. |
| Agent / Caller labels | Not built. |

## 2. Target

```
Call app speaker ─► ClearNAI Speaker ─► caller path: RNNoise → BVC → studio ─► agent's headset
                                                          │ tap ③ Caller (after cleanup)
Real mic ─► echo removal → gate → boost → Hush ─┬─► accent → AutoGain → studio → limiter ─► ClearNAI Mic ─► call app
                                               │ tap ① Agent (after cleanup, before accent)
                                               ▼                                    ▼
                                          ring "Agent"                         ring "Caller"
                                               └──────► one transcript worker ◄──────┘
                                                        one model, one Segmenter per side
                                                               │ line = (start time, side, text)
                                                               ▼
                                     merged by time ─► Transcript tab (two colours) + one file per call
```

## 3. Decisions

1. **Label by stream, not by voice.** The mic and the call audio are separate streams, so who spoke is known for
   certain. A diarization (speaker recognition) model is only needed when several people share one stream, such as a
   conference call. That is out of scope for 1:1 calls.
2. **One model, two queues.** One `AsrModel` loaded once. The worker keeps a `Segmenter` per side and transcribes
   finished segments from either side in arrival order. It runs below normal priority, as today, so it never competes
   with the mic, Hush or accent threads.
3. **Tap points.**
   - Agent: after Hush, before accent. The text is what the agent actually said, cleaned. This is today's tap.
   - Caller: after the caller-side cleanup, in `spawn_speaker_pipeline`, before render. A second `TranscriptStage`
     there only copies frames into its own ring and never changes the audio.
4. **One clock.** Both stages stamp frames with a shared engine clock (`dsp_core::clock`), so lines from both sides
   interleave correctly: `[00:12] Agent: …` / `[00:15] Caller: …`.
5. **One file per call.**
   - A call starts when caller audio becomes non-silent.
   - It ends after 2 minutes of silence on both sides, when call audio stops, or when the transcript is switched off.
   - File: `transcripts/YYYY-MM-DD_HH-MM_call.txt`, plus `.json` with `{start, side, text}` per line for export.
6. **Echo guard.** Caller audio leaking from speakers into the mic would also be transcribed as Agent. Echo removal
   handles most of it. In addition, drop an Agent segment that overlaps a Caller segment in time and whose level is
   much lower (more than 12 dB below the caller's).
7. **Load.** Two streams plus accent is heavy on 4–6 core PCs. The worker skips new segments while accent reports
   load (KRIS-44), and marks the gap in the file as "[transcript paused]".
8. **Privacy (KRIS-60).** A keep-for-N-days setting (default 30), a "Delete all transcripts" button, and the BPO
   signs off on storing call text. Nothing is uploaded.
9. **Caller audio quality.** Telephony audio is 8–16 kHz. The model works at 16 kHz, so no extra step is needed.
   Measure WER on real BPO calls before release.

## 4. Changes by file

| File | Change |
|---|---|
| `crates/asr-onnx/src/pipeline.rs` | `Side { Agent, Caller }`. `start` returns two stages (agent, caller) sharing one worker. `TranscriptLine` gets `side`. One `Segmenter` per side. Per-call file and JSON writer. Echo guard. |
| `app/src/engine.rs` | Pass the caller stage into `spawn_speaker_pipeline` and call it after cleanup. Keep the agent stage where it is. |
| `app/src/ipc.rs`, `remote.rs` | `TranscriptWire` lines carry the side. |
| `app/src/gui/tabs.rs` | Transcript tab: "Agent" / "Caller" labels in two colours. Export keeps the labels. |
| `app/src/settings.rs` | `transcript_keep_days`, delete-all action. |

## 5. Plan and effort

| Step | Effort |
|---|---|
| Caller tap, second segmenter, side labels, merge by time | 1.5 d |
| One file per call (text + JSON) | 0.5 d |
| Two-colour Transcript tab and export | 0.5 d |
| Echo guard and tests (golden audio for both sides) | 0.5 d |
| **Total** | **about 3 d** |
| Caller audio via ClearNAI Speaker endpoint, no second cable (KRIS-51) | +1–1.5 d (driver) |

## 6. Tests

- Unit: two synthetic sides with known segments; check line order and labels in the merge, the per-call file split, and the echo guard.
- Golden: the existing `asr-onnx` golden test, run once per side.
- Live: a 10-minute test call with two people. Every line has the right label, there are no duplicate lines from
  echo, the file splits per call, and CPU stays within budget with accent on.
