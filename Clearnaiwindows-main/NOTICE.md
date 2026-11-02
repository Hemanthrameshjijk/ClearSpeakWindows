# Third-party components

ClearNAI is built on the open-source components below. Licence names are the ones published by each project; check
the project itself for the authoritative text before redistributing.

| Component | Used for | Licence |
|---|---|---|
| ONNX Runtime (`ort`) | Running the accent, transcript and noise models | MIT |
| RNNoise / `nnnoiseless` | Light noise removal | BSD-3-Clause |
| DeepFilterNet | Optional noise-removal backend | MIT or Apache-2.0 |
| WeSpeaker / WeSep | Speaker embedding experiments | Apache-2.0 |
| iced | Window and controls | MIT |
| `tray-icon`, `global-hotkey` | Tray icon, mute hotkey | MIT or Apache-2.0 |
| `windows` (Microsoft) | Windows audio and system calls | MIT or Apache-2.0 |
| VB-CABLE (VB-Audio) | Virtual microphone cable | Donationware, personal use; not redistributable without permission |

## To confirm before a public release

These were not verified in this repository and need a decision from the owner:

- `weya_nc.dll` (Hush noise model): redistribution terms.
- Accent conversion models and the transcript (ASR) model: licence and attribution text.
- VB-CABLE: link users to the vendor's download page instead of bundling it, unless VB-Audio grants permission.
- The ClearNAI source licence itself (no `LICENSE` file is added here because that is the owner's choice).
