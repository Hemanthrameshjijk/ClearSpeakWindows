# Automatic call recording

Every call is recorded without the agent doing anything. Each call gets three files in
`%LOCALAPPDATA%\ClearSpeak\transcripts`, sharing one name:

```
2026-11-26_10-15-02_call_VICIdial_+919876543210.wav    audio: agent left, caller right (16 kHz stereo, ~230 MB/hour)
2026-11-26_10-15-02_call_VICIdial_+919876543210.txt    transcript, first line: Call <time>  App: <app>  Caller: <number>
2026-11-26_10-15-02_call_VICIdial_+919876543210.jsonl  same lines for export tools
```

The number is in the name only when it is known (VICIdial). "Keep saved transcripts" (7/30/90 days/forever) and
"Delete all" in the Transcript tab cover all three. The app log never contains the caller's number.

## Call recording tab (full window)

"Record calls automatically" (on by default), and **How calls are found**, one of three:

1. **VICIdial agent screen (no password).** Reads the VICIdial page open in the browser through Windows UI
   Automation (`app/src/vicidial_screen.rs`): its "Live Call" / "No Live Call" mark and the `phone_number` field. The
   page must be the visible tab. Tested on a mock page in Edge (`cargo run --example vicidial_screen_check`); the names
   still need checking on the real page with `tools/eyebeam_probe.ps1`.
2. **Caller's voice** (default). See the table below. Without call audio set up, the microphone is used.
3. **VICIdial API (needs an API password).** Server address, API user, password (encrypted on Save) and the agent's
   VICIdial ID, typed into the tab.

4. **eyeBeam call (no VICIdial).** A recording per eyeBeam call: starts when eyeBeam opens the microphone, pauses
   when it lets go (hold), stops after 2 min free. Other call apps are ignored. eyeBeam, X-Lite and Bria count;
   `CLEARSPEAK_CALL_APPS=name1,name2` replaces that list (another softphone, or a test).

A "Now:" line shows what is being used right now (for example "VICIdial page not found ... the caller's voice is used
meanwhile"). When 1 or 3 has no answer, 2 is used, and a call already being recorded goes on for up to 2 minutes.

## When a recording starts and stops

`app/src/call_session.rs`:

| Signal | Used when | Start | Hold | Stop |
|---|---|---|---|---|
| **VICIdial** | the four settings below are filled in and the dialer answers | VICIdial puts a customer through (`INCALL`) | customer parked: paused, same file | customer gone (READY / PAUSED / wrap-up / hung up), or the next customer (new file) |
| **Caller voice** | the call app's speaker goes through ClearSpeak (call audio set up) | a call app holds the mic **and** the caller side has sound | call app lets go of the mic: paused | 60 s with no caller sound, or the call ends |
| **Microphone** | otherwise | a call app (eyeBeam, Teams, ...) holds the mic for 3 s | mic free for 5 s: paused | mic free for 2 min |

With VICIdial the agent's eyeBeam stays on one line to the dialer all shift. Only VICIdial knows where one customer
ends and the next begins, so set it up for those agents. Idle time between customers is never recorded.

Pre-roll: a recording starts with what was said just before the call was recognised: 2 s for VICIdial, 10 s
otherwise (the mic is recognised 3 s late, and the caller speaks after the agent's greeting).

## VICIdial API setup without the window (per PC)

Normally typed into the Call recording tab. For a mass deployment, add these to
`%LOCALAPPDATA%\ClearSpeak\settings.json` while ClearSpeak is closed (with `"record_method": 3`):

```json
"vicidial_url": "http://<dialer>/vicidial/non_agent_api.php",
"vicidial_api_user": "<API user with Non-Agent API access>",
"vicidial_api_pass": "<its password>",
"vicidial_agent_user": "<this agent's VICIdial user ID>"
```

At the next start the password is encrypted for that Windows user (`dpapi1:...`) and the log says
`[vicidial] API password encrypted in the settings file`. The app asks `function=agent_status` once a second. The
log shows `[vicidial] customer connected` / `no customer (...)` / `not answering: ...`. If the dialer stops answering
mid-call, the recording is kept for up to 2 minutes (or until the call app lets go of the mic).

## Caller side

The right channel and the Caller lines need the call app's speaker to go through ClearSpeak (Devices → call audio).
Without it the log says `only the agent's side is recorded` and the Transcript tab shows a warning.

## Known limits

- **Microphone signal:** two sources, combined (`call_log::mic_users`): Windows' privacy record
  (`CapabilityAccessManager\ConsentStore\microphone`) and the audio sessions capturing from each microphone right now.
  The record alone is not enough: on the dev PC (build 26300) Windows stopped updating it on 11 Nov 2026. The audio
  sessions see a program within a second (checked with `cargo run --example mic_users_check`).
- **eyeBeam caller ID / buttons:** not read yet. `tools/eyebeam_probe.ps1` (read-only) shows what eyeBeam exposes
  during a test call. Run it on an agent PC and send back the file it writes to the Desktop.
- Storing full phone numbers needs the BPO's sign-off (KRIS-60).
