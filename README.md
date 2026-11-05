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
- It can't type into apps running as administrator.

## Build

Needs the .NET 9 SDK.

```
dotnet publish src -c Release
```

The exe lands in `src/bin/Release/net9.0-windows/win-x64/publish/`.

GitHub Actions builds every push and pull request; the exe is attached to each run under **Artifacts**. Pushing a tag like `v1.0.1` builds it and publishes a release with `WispX.exe` attached.

## Layout

```
src/
  App.xaml.cs         startup, dictation flow
  Audio/              mic recording, sounds
  Input/              configurable hotkey hook, pasting, Win32 calls
  Transcription/      ChatGPT transcribe + Codex login
  UI/                 overlay pill, tray, Options pages
  Setup/              self-install, settings
  Assets/             icon
```
