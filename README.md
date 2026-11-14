# WispX

[![Build](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml/badge.svg)](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/PeytonWDYM/WispX)](https://github.com/PeytonWDYM/WispX/releases/latest)

Push-to-talk dictation for Windows and Linux (X11 or Wayland, with KDE Plasma support). Hold **Ctrl + Win**, speak, and let go. Your words are typed wherever the cursor is. **Listen** turns a meeting, call or video playing on your computer into a formatted transcript and notes.

<p align="center"><img src="docs/images/pill.png" alt="The WispX recording pill" width="172"></p>

WispX transcribes with ChatGPT, just like the Dictate button in the ChatGPT and Codex apps. It reuses the sign-in the Codex app already saved on your PC, so you don't need an API key or a separate login.

## Install

On Windows, you need Windows 10 or 11 and the [Codex app](https://openai.com/codex/) signed in with your ChatGPT account.

1. Download `WispX.exe` from the [latest release](https://github.com/PeytonWDYM/WispX/releases/latest).
2. Run it. WispX installs itself, adds a Start menu entry, and starts in the system tray.

The exe isn't code-signed, so SmartScreen may warn you. Click **More info → Run anyway**. To update, run the newer `WispX.exe`.

On Linux, unpack `WispX-linux-x86_64.tar.gz` from the same release somewhere permanent, then run `WispX`. It adds an app launcher entry. Sign in to the Codex app or CLI with your ChatGPT account first.

Linux needs ALSA's `libasound.so.2`. Listen records computer audio through `parec` and lists devices with `pactl` (both from `pulseaudio-utils`, or `libpulse` on Arch), with PulseAudio or PipeWire's `pipewire-pulse`; without them Listen says so and dictation is unaffected. On Wayland, global shortcuts need `xdg-desktop-portal` 1.18+ and a GlobalShortcuts backend, such as Plasma 6's `xdg-desktop-portal-kde`. The pill uses layer shell where available (including KWin), or XWayland otherwise. A StatusNotifierItem tray shows the tray menu; without one, launch WispX again to open Options. Lowering other audio while dictating uses `pactl` (from `pulseaudio-utils` or `libpulse`) with PulseAudio or PipeWire's `pipewire-pulse`, plus PipeWire's `pw-cli` so apps' remembered volumes stay as they were; without them, dictation works and other audio is left alone. `WispX --diagnose` reports desktop support, and `WispX --uninstall` removes its launcher and sign-in entries.

## Use

| Keys | Action |
| --- | --- |
| Hold **Ctrl + Win** | Record while held, then type the text when released |
| **Ctrl + Win**, then **Space** | Hands-free. Press Ctrl + Win again to finish |
| **Esc** | Cancel |
| **Ctrl + Shift + F8** | Listen: start, or stop and write notes. While Listen records, **Esc** discards the session |

Click the tray icon to open **Options**. From there you can change the shortcut, microphone, language, sounds, and startup behavior.

While you dictate, other apps' audio drops to 20% of its volume and returns to its earlier level as soon as recording ends or is cancelled. Volumes you change meanwhile are kept. Turn this off under **Options → General → Lower other audio while dictating**.

The **Stats** page shows word counts and dictation speed, with a chart of the last 30 days. It saves only counts and spoken time in `stats.json` next to your settings, never the text; you can reset it anytime.

<p align="center"><img src="docs/images/options-stats.png" alt="Options window showing the Stats page" width="640"></p>

<p align="center"><img src="docs/images/options-shortcut.png" alt="Options window showing the Shortcut page" width="640"></p>

### Listen

**Options → Listen** records what plays on one audio output (every app on it, not one window) and, if you turn on **Include my microphone**, your microphone too. Wear headphones when you include the microphone: WispX doesn't cancel echo, so speakers would be picked up twice. Other apps keep their normal volume.

While it listens, WispX sends the audio in chunks of 20 to 40 seconds, cut at pauses, to ChatGPT's transcription endpoint and shows a rough draft. The pill turns into a headphone pill with a blue dot. Press the Listen shortcut again, **Stop & summarize**, or the tray's **Stop listening** to finish: WispX waits for the last chunks, formats the transcript (punctuation, paragraphs, dictionary spellings, timestamps; nothing added) in sections of about ten minutes, then writes a summary with a TL;DR, decisions and action items. Owners appear only when the transcript names them.

Nothing is written to disk until you choose **Save notes**, which saves Markdown wherever you pick. **Copy** copies the summary or transcript. Notes stay in WispX until you close them, even after closing Options, and WispX won't start a new session over unsaved notes. Parts that couldn't be transcribed (a lasting outage, or an upload backlog of more than 12 chunks) are marked in the transcript and the notes say they're incomplete. If formatting fails, you get the original transcript, labeled; if the summary fails, the transcript is kept and **Try again** retries it. The original transcript is always one click away. Formatting and summaries need Codex signed in with ChatGPT. Sessions end on their own after four hours. If the summary arrives after you saved, the notes show as unsaved again until you save the new version. **Discard**, closing the notes, or quitting stops any formatting or summary requests still to come.

Listen never switches devices on its own. If the output or microphone goes away, or the sound server moves the recording to another device, Listen stops and writes notes from what it heard, saying it stopped early. It does the same if no audio arrives for more than 10 seconds (for example, the computer slept) or WispX falls more than 20 seconds behind the audio.

One recording runs at a time: dictation and the microphone test wait while Listen records, and Listen won't start during a dictation. Listen never types into other apps.

Quit opens Listen while a session is active or recovered text is unsaved; save or explicitly discard it first. Sessions with no recovered speech can be replaced immediately. Transcription errors are reported separately from silence.

### Linux shortcuts and pasting

On **X11**, shortcuts work as above; Win is the Super/Meta key. On **Wayland**, the desktop owns the shortcuts. Plasma can't bind a modifier-only chord, so the defaults are **Ctrl + Meta + D** to hold-to-talk, **Ctrl + Meta + Space** for hands-free (press either shortcut again to finish), and **Ctrl + Meta + Backspace** to cancel (it also discards a Listen session that's recording). Listen asks for **Ctrl + Shift + F8**. **Change…** opens the desktop's shortcut settings, and Options shows the keys the desktop actually assigned. Without the portal, use **Start listening** in Options or the tray.

Plasma asks you to confirm shortcuts and allow keyboard remote control on first start. WispX pastes only if the original window still has focus, using XTEST on X11 or KWin focus checks and the RemoteDesktop portal on Plasma Wayland. Other Wayland desktops fall back to copying for you to paste manually. Terminals get Ctrl + Shift + V. After a paste, your previous clipboard is restored unless you copied something newer.

### Cleanup

Cleanup is on by default. It runs a second pass that fixes punctuation, removes filler words, and keeps what you meant when you correct yourself mid-sentence. If cleanup fails or takes too long, WispX types the original transcript.

- **Use nearby text** (Windows only) sends up to 2,000 characters around your cursor so the new text fits what's already there. It never reads password fields. In VS Code, turn on **Editor: Accessibility Support** so WispX can see the text. Linux cleanup runs without nearby text.
- **Dictionary** holds names, file paths, and code symbols that cleanup should spell exactly.

<p align="center"><img src="docs/images/options-cleanup.png" alt="Options window showing the Cleanup page" width="640"></p>


## Good to know

- WispX uses ChatGPT's private transcription endpoint. It isn't a public API, so it can change without notice, and using it may go against OpenAI's terms.
- ChatGPT keeps the audio for 30 days, the same as its built-in dictation.
- Listen sends a long recording as many transcription requests, and its transcript text to the Codex responses endpoint for formatting and the summary. How much sustained use the private endpoints allow hasn't been established; rate limits show up as untranscribed gaps, never as silently missing text.
- WispX reads your Codex sign-in from `%USERPROFILE%\.codex\auth.json` (`~/.codex/auth.json` on Linux). When the token expires, WispX refreshes it so Codex stays signed in.
- Windows doesn't let WispX type into apps running as administrator.

## Build from source

Requires Rust 1.92+ and, on Windows, the Visual Studio C++ build tools with the Windows SDK.

On Linux, install ALSA development files and `pkg-config`: `libasound2-dev pkg-config` on Debian/Ubuntu, `alsa-lib pkgconf` on Arch, or `alsa-lib-devel` on Fedora. Cargo selects the platform from the build target.

```sh
cargo build --release              # target/release/WispX.exe (Windows) or WispX (Linux)
target/release/WispX.exe --portable  # run without installing
```

Explicit targets:

```sh
cargo build --release --target x86_64-unknown-linux-gnu
cargo build --release --target x86_64-pc-windows-msvc  # on Windows
cargo build --release --target x86_64-pc-windows-gnu   # with MinGW
```

Before opening a PR, run the checks:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`scripts/e2e.ps1` runs the end-to-end test. It needs Microsoft Edge, `ffmpeg` and `ffprobe` on PATH, and a signed-in Codex app. The test synthesizes speech and transcribes it with the real endpoint. Then it checks cleanup and dictionary correction, tests reading nearby text in a throwaway Edge profile, and screenshots the UI. Everything it produces goes to `artifacts/e2e/`. `scripts/context-editors.ps1` runs a similar nearby-text check against VS Code and Notepad.

On Linux, `bash scripts/ci-linux.sh` runs the checks, builds the Linux app, paints the pill offscreen, and cross-compiles the Windows exe. It downloads Rust, MinGW and any missing ALSA development packages into user-owned directories without sudo. `cargo test --lib audio::pulse -- --ignored` records a tone from a throwaway null sink and checks that removing or moving the device ends the capture. These tests require both `PULSE_SERVER` and `WISPX_TEST_PULSE_SERVER` set to the same private test server; they refuse to use an implicit desktop server. `WISPX_LISTEN_SHOTS=artifacts/listen-shots cargo test --lib ui::tests -- --ignored` renders each Listen state offscreen, with fixture text, for review. `scripts/linux-smoke.sh` checks startup and UI in a private headless Plasma session; `cargo test --test linux_nested -- --ignored --nocapture --test-threads=1` exercises shortcuts and pasting there.

CI builds every push and pull request. Pushing a `v*` tag publishes a release.

### Project layout

```
src/
  app.rs             dictation controller
  hotkey*.rs         shortcut model and key state machine
  audio/             recording, Listen capture (WASAPI loopback, PulseAudio monitors), Opus/WebM, cue sounds
  transcribe/        Codex auth, ChatGPT transcription, cleanup, responses
  listen/            Listen sessions: chunking, ordered assembly, formatting, summary
  ui/                Options window logic
  platform/windows/  keyboard hook, tray, pill overlay, installer
  platform/linux/    X11/portal shortcuts, clipboard, paste, tray, pill, autostart
ui/                  Slint layouts for Options
scripts/             E2E tests, Linux smoke test and cross-build
```

## Credits

The Options window is built with [Slint](https://slint.dev) under the [Slint Royalty-free License](https://slint.dev/license/royalty-free/2.0/).

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
