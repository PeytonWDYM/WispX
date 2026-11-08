# WispX

[![Build](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml/badge.svg)](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/PeytonWDYM/WispX)](https://github.com/PeytonWDYM/WispX/releases/latest)

Hold **Ctrl + Win**, talk, let go. What you said gets typed wherever your cursor is.

ChatGPT does the transcription, the same way the Dictate button in the ChatGPT and Codex apps does. WispX uses the login the Codex app already saved on your PC, so there's no API key and no extra sign-in.

## Install

1. Download `WispX.exe` from [Releases](../../releases/latest).
2. Run it. It installs itself to `%LOCALAPPDATA%\Programs\WispX`, adds WispX to the Start menu, and starts.

You need Windows 10 or 11 and the [Codex app](https://openai.com/codex/) signed in with your ChatGPT account.

The exe isn't code-signed, so SmartScreen may warn you. Click **More info → Run anyway**.

To update, download the new `WispX.exe` and run it.

## Use

| Keys | What it does |
| --- | --- |
| Hold Ctrl + Win | Records. Let go and it types the text. |
| Ctrl + Win, then Space | Hands-free. Press Ctrl + Win again to finish. |
| Esc | Cancel |

Left-click the tray icon to open **Options**, where you can change the shortcut, microphone, language, sounds, idle pill, and start with Windows. Ctrl + Win is the default shortcut.

The **Cleanup** page turns on a second pass that fixes punctuation, drops filler words, and keeps what you meant when you correct yourself. It's on by default and needs Codex signed in with a ChatGPT account (API-key sign-ins don't count). It uses `gpt-6-luna` with the same instructions as the Codex app's own dictation cleanup. If it fails, takes longer than 8 seconds, or the transcript is over 4,000 characters, you get the original transcript instead.

**Use nearby text** sends up to 2,000 characters around your cursor along with the cleanup, so punctuation and wording flow with what's already there. WispX reads it through Windows UI Automation when you start recording. It never reads password fields or its own windows, and never logs the text. Apps that don't expose their text just get no context. You can turn it off separately.

Capture runs on one COM worker throughout recording and transcription. Cleanup takes an already-completed result without waiting, so a slow or hung accessibility provider adds no hotkey, pill, or paste latency. If Chromium initially exposes a container while its accessibility tree is starting, that worker retries for at most two seconds while the original window stays focused. This gives the asynchronous tree time to expose its field; cleanup still never waits. Text ranges stay inside the focused field, and `[cursor]` marks the insertion point for cleanup. WispX adds a leading word separator when the known caret needs one, preserving existing whitespace and opening brackets/quotes. Editors must expose their actual text and caret: VS Code's default native edit context can expose a placeholder until **Editor: Accessibility Support** is on.

Add names, file paths, and code symbols to the **Dictionary** so cleanup spells them exactly. It holds up to 200 entries, and the first 100 go with each cleanup.

The tray menu also has copy last transcript, How to use, and uninstall.

## Notes

- WispX calls ChatGPT's `/backend-api/transcribe` endpoint. It's not a public API, so it can change or break without warning, and using it may go against OpenAI's terms.
- ChatGPT keeps the audio for 30 days, the same as the app's own dictation.
- WispX reads `%USERPROFILE%\.codex\auth.json`. If the token has expired, it refreshes it and writes the new one back so Codex stays signed in.
- Uploads use mono Opus in WebM (`audio/webm`), encoded from the microphone's 16 kHz PCM at 32 kbps with 20 ms voice frames. A ten-minute recording is about 2.6 MB including the container, versus 19.2 MB as WAV. Encoding uses pure-Rust `opus-rs`, with no additional native build tools.
- Network failures and HTTP 408/500/502/503/504 get up to three attempts, starting with 500 ms and 1 s delays. WispX respects longer `Retry-After` delays up to 10 s; token refresh has its own retry. Server error codes produce specific messages for short/oversized recordings, unsupported audio, expired/revoked tokens, rate limits, and busy models.
- It can't type into apps running as administrator.

## Build

Needs Rust 1.92 or newer. On Windows, install Visual Studio's C++ build tools and Windows SDK.

```
cargo build --release
```

The exe lands in `target/release/WispX.exe`. Run it with `--portable` to skip installation while developing.

`powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-windows-gnu.ps1` verifies the GNU target on Windows with a downloaded MinGW GCC toolchain and the same resources used by CI.

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --lib
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/e2e.ps1
```

The E2E script needs Microsoft Edge, `ffprobe`, `ffmpeg`, and a Codex ChatGPT login. It launches its own Edge with a temporary profile and local HTML fixture, checks exact mid-text caret context in a textarea, input, and contenteditable, and excludes a password field. The first capture uses the page's initial focus, before looking up any named fields. The occlusion flag keeps the fixture available while other windows are active; accessibility is not forced on. The script closes its Edge window afterwards.

It also synthesizes a repeatable 16 kHz mono WAV, uploads Opus/WebM, checks live transcription/dictionary cleanup, and verifies the reported "Basically, we're testing the" continuation with and without existing whitespace. The native Edit fixture covers password/own-window exclusion, a result completing after the old 250 ms deadline, and nonblocking consumption of a pending capture. Pill/Options screenshots and sound exports remain covered. Artifacts go in `artifacts/e2e/`: `context/*.json` records provider paths, timings, and synthetic text; `selftest.log` records endpoint results; `selftest.webm`, `ffprobe.json`, `decoded.wav`, and `ffmpeg.log` independently verify the audio upload. This stands in for microphone dictation; it does not exercise a physical mic or paste into a user's app.

`scripts/context-editors.ps1` separately investigates an isolated VS Code profile/extensions directory (default and accessible editor modes) and Windows 11 Notepad with a temporary file. It refuses to run Notepad while an existing instance is open. These development entry points bypass installation, instance events, tray, and hotkeys. To inspect another synthetic fixture, run `WispX.exe --context-probe request.json`, where the JSON contains `hwnd`, a field `name` or UIA `control_type`, optional character `caret`, and an `out` JSON path. Only point this mode at fixtures you launched: unlike recording, it deliberately logs text. `--cleanup-probe output.json` repeats fixed continuation prompts against the live endpoint.

On Linux, `bash scripts/ci-linux.sh` verifies the portable core and cross-builds `target/x86_64-pc-windows-gnu/release/WispX.exe`. It downloads Rust and extracts MinGW GCC packages into user-owned directories, along with ALSA development packages if needed. No sudo, CMake, NASM, or Perl is needed. Linux only builds the core for now; it doesn't run a dictation app.

GitHub Actions builds every push and pull request; the exe is attached to each run under **Artifacts**. Pushing a tag like `v1.0.1` builds it and publishes a release with `WispX.exe` attached.

## Layout

```
Cargo.toml, build.rs  crate, Slint compilation, Windows resources
assets/              original icon, DPI manifest, version information
ui/                  portable Fluent Options layout
src/
  main.rs            command-line entry points and startup
  app.rs             dictation controller
  settings.rs        existing settings.json, atomic saves and notifications
  hotkey.rs          shortcut model and settings serialization
  hotkey_machine.rs  physical-key state machine and capture
  audio/             CPAL recording, PCM, Opus/WebM uploads, WAV cue sounds
  transcribe/        Codex login, ChatGPT transcription with retries, cleanup pass
  ui/                Options behavior and screenshot tour
  platform/windows/  hook, clipboard, tray, pill, install, startup, instance events
scripts/             repeatable E2E and sudo-free Linux cross-build
```

Options is built with [Slint](https://slint.dev), used under the [Slint Royalty-free License](https://slint.dev/license/royalty-free/2.0/). The General page credits it too.

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
