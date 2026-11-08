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

The E2E script needs `ffprobe` and `ffmpeg` on PATH. It makes a repeatable 16 kHz mono WAV with Windows speech synthesis, encodes and transcribes it with your real Codex login, checks the transcript, walks the pill through its states, takes Options screenshots, and exports the sounds. Its files go in `artifacts/e2e/`: `selftest.log` records the transcript and WebM versus equivalent WAV size, `selftest.webm` is the exact upload, `ffprobe.json` records independent container inspection, and `decoded.wav`/`ffmpeg.log` verify clean decoding. These development modes don't install, create a tray icon, register a hook, or contact an existing WispX instance.

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
  transcribe/        Codex login, token refresh and ChatGPT client
  ui/                Options behavior and screenshot tour
  platform/windows/  hook, clipboard, tray, pill, install, startup, instance events
scripts/             repeatable E2E and sudo-free Linux cross-build
```

Options is built with [Slint](https://slint.dev), used under the [Slint Royalty-free License](https://slint.dev/license/royalty-free/2.0/). The General page credits it too.

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
