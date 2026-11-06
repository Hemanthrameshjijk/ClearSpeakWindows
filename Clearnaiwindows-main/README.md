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

## License keys and profile

ClearNAI checks an offline license key (a signed name, email and expiry) against the public key built into the app
(`app/src/license_key.rs`). Without a key the app runs a 14-day free trial counted from the first launch.

The owner makes keys with the `licensegen` tool (`crates/license`):

    cargo run -p license --bin licensegen -- newkey <secret-file>              (once; keep the file private and backed up)
    cargo run -p license --bin licensegen -- make <secret-file> "Name" "email" [days]

The secret key must never be committed or shipped. The user pastes the key on the Profile page.
