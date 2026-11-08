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

The **Cleanup** page controls a second pass that fixes punctuation, removes filler words, and keeps your corrected intent. It is on by default and requires Codex signed in with a ChatGPT account; API-key sign-ins do not qualify. Cleanup uses `gpt-6-luna` through Codex's private responses endpoint. If it fails, takes more than eight seconds, returns no text, or your transcript exceeds 4,000 characters, WispX uses the original transcript.

**Use nearby text** is also on by default. When recording starts, WispX reads up to 2,000 characters around your cursor through Windows UI Automation and sends them only with cleanup. Password fields and WispX's own windows are excluded, and captured text is never logged. Where an app exposes a whole value without a caret position, WispX uses the value's tail. Apps that do not expose text, or respond too slowly, simply provide no context. You can turn this off independently of cleanup.

Add canonical names, file paths, and code symbols to the **Dictionary** so cleanup can spell them exactly. WispX saves up to 200 entries and sends the first 100 with each cleanup request. Entries are trimmed and exact duplicates are ignored; casing and punctuation are preserved.

The tray menu also has copy last transcript, How to use, and uninstall.

## Notes

- WispX calls ChatGPT's `/backend-api/transcribe` endpoint. It's not a public API, so it can change or break without warning, and using it may go against OpenAI's terms.
- ChatGPT keeps the audio for 30 days, the same as the app's own dictation.
- WispX reads `%USERPROFILE%\.codex\auth.json`. If the token has expired, it refreshes it and writes the new one back so Codex stays signed in.
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

The E2E script makes a repeatable 16 kHz mono WAV with Windows speech synthesis, transcribes it with your real Codex login, then runs real cleanup with a `WispX` dictionary entry and fixed surrounding text. It logs raw/cleaned text and timing, verifies the historical `Wispecs` recognition mistake is corrected, and checks caret context against a small Win32 EDIT control owned by the selftest. It also walks the pill through its states, takes screenshots of all five Options pages (including `options/4-cleanup.png`), and exports the sounds. Its files go in `artifacts/e2e/`, with results in `selftest.log`. These development modes don't install, create a tray icon, register a hook, or contact an existing WispX instance. The context test reads only its own fixed text.

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
  audio/             CPAL recording, PCM, WAV encoding, synthesized sounds
  transcribe/        Codex login, token refresh and ChatGPT client
  ui/                Options behavior and screenshot tour
  platform/windows/  hook, clipboard, tray, pill, install, startup, instance events
scripts/             repeatable E2E and sudo-free Linux cross-build
```

Options is built with [Slint](https://slint.dev), used under the [Slint Royalty-free License](https://slint.dev/license/royalty-free/2.0/). The General page credits it too.

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
