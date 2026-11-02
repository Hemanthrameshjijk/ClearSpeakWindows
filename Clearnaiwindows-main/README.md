# ClearNAI

Windows microphone cleaning: removes background noise and voices, can convert an accent live, and keeps a live
transcript. It works as a virtual microphone, so any call app can use it.

## Using it
- Open **ClearNAI** (exe or Start menu): a small card with your voice level, **Cancel noise**, **Convert my accent**,
  **Live transcript** and **Mute microphone**. Closing it keeps ClearNAI running in the tray.
- Tray icon: click for the small bar, right-click for the menu. Settings (the full window) open behind a password.
- **Mic distance**: Close for a boom mic, Normal or Far for an earbud or far mic. **Set for my mic** listens for 5 seconds
  and picks it for you.
- Pick **CABLE Input** as the virtual microphone target, and choose **CABLE Output** as the microphone in your call app.

## Build
- `cargo build --release -p clearnai-app --bins`, then `makensis installer.nsi` from `app/` (needs NSIS).
- Tests: `cargo test --workspace`.
- Models are not in git; see `docs/PACKAGING.md`.

Third-party components and open licence questions: `NOTICE.md`.
