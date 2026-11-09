# WispX

[![Build](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml/badge.svg)](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/PeytonWDYM/WispX)](https://github.com/PeytonWDYM/WispX/releases/latest)

Push-to-talk dictation for Windows. Hold **Ctrl + Win**, speak, and let go. Your words are typed wherever the cursor is.

<p align="center"><img src="docs/images/pill.png" alt="The WispX recording pill" width="172"></p>

WispX transcribes with ChatGPT, just like the Dictate button in the ChatGPT and Codex apps. It reuses the sign-in the Codex app already saved on your PC, so you don't need an API key or a separate login.

## Install

You need Windows 10 or 11 and the [Codex app](https://openai.com/codex/) signed in with your ChatGPT account.

1. Download `WispX.exe` from the [latest release](https://github.com/PeytonWDYM/WispX/releases/latest).
2. Run it. WispX installs itself, adds a Start menu entry, and starts in the system tray.

The exe isn't code-signed, so SmartScreen may warn you. Click **More info → Run anyway**. To update, run the newer `WispX.exe`.

## Use

| Keys | Action |
| --- | --- |
| Hold **Ctrl + Win** | Record while held, then type the text when released |
| **Ctrl + Win**, then **Space** | Hands-free. Press Ctrl + Win again to finish |
| **Esc** | Cancel |

Click the tray icon to open **Options**. From there you can change the shortcut, microphone, language, sounds, and startup behavior.

<p align="center"><img src="docs/images/options-shortcut.png" alt="Options window showing the Shortcut page" width="640"></p>

### Cleanup

Cleanup is on by default. It runs a second pass that fixes punctuation, removes filler words, and keeps what you meant when you correct yourself mid-sentence. If cleanup fails or takes too long, WispX types the original transcript.

- **Use nearby text** sends up to 2,000 characters around your cursor so the new text fits what's already there. It never reads password fields. In VS Code, turn on **Editor: Accessibility Support** so WispX can see the text.
- **Dictionary** holds names, file paths, and code symbols that cleanup should spell exactly.

<p align="center"><img src="docs/images/options-cleanup.png" alt="Options window showing the Cleanup page" width="640"></p>

## Good to know

- WispX uses ChatGPT's private transcription endpoint. It isn't a public API, so it can change without notice, and using it may go against OpenAI's terms.
- ChatGPT keeps the audio for 30 days, the same as its built-in dictation.
- WispX reads your Codex sign-in from `%USERPROFILE%\.codex\auth.json`. When the token expires, WispX refreshes it so Codex stays signed in.
- Windows doesn't let WispX type into apps running as administrator.

## Build from source

Requires Rust 1.92+ and, on Windows, the Visual Studio C++ build tools with the Windows SDK.

```sh
cargo build --release              # target/release/WispX.exe
target/release/WispX.exe --portable  # run without installing
```

Before opening a PR, run the checks:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --lib
```

`scripts/e2e.ps1` runs the end-to-end test. It needs Microsoft Edge, `ffmpeg` and `ffprobe` on PATH, and a signed-in Codex app. The test synthesizes speech and transcribes it with the real endpoint. Then it checks cleanup and dictionary correction, tests reading nearby text in a throwaway Edge profile, and screenshots the UI. Everything it produces goes to `artifacts/e2e/`. `scripts/context-editors.ps1` runs a similar nearby-text check against VS Code and Notepad.

On Linux, `scripts/ci-linux.sh` checks the portable core and cross-compiles the Windows exe without sudo.

CI builds every push and pull request. Pushing a `v*` tag publishes a release.

### Project layout

```
src/
  app.rs             dictation controller
  hotkey*.rs         shortcut model and key state machine
  audio/             recording, Opus/WebM encoding, cue sounds
  transcribe/        Codex auth, ChatGPT transcription, cleanup
  ui/                Options window logic
  platform/windows/  keyboard hook, tray, pill overlay, installer
ui/                  Slint layouts for Options
scripts/             E2E test and Linux cross-build
```

## Credits

The Options window is built with [Slint](https://slint.dev) under the [Slint Royalty-free License](https://slint.dev/license/royalty-free/2.0/).

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
