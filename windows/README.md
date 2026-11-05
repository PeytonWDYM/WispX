# WispX for Windows

[![Build](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml/badge.svg)](https://github.com/PeytonWDYM/WispX/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/PeytonWDYM/WispX)](https://github.com/PeytonWDYM/WispX/releases/latest)

Hold **Ctrl + Win**, talk, let go. What you said gets typed wherever your cursor is.

A Rust port is being tested alongside this app. See [On Windows](../README.md#on-windows). This page is about the original.

ChatGPT does the transcription, the same way the Dictate button in the ChatGPT and Codex apps does. WispX uses the login the Codex app already saved on your PC, so there's no API key and no extra sign-in.

## Install

1. Download `WispX.exe` from [Releases](https://github.com/PeytonWDYM/WispX/releases/latest).
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

The tray icon has: copy last transcript, sounds, idle pill, start with Windows, and uninstall.

## Notes

- WispX calls ChatGPT's `/backend-api/transcribe` endpoint. It's not a public API, so it can change or break without warning, and using it may go against OpenAI's terms.
- Your audio is uploaded to chatgpt.com. How long this private endpoint keeps it isn't publicly documented, so WispX can't promise anything about retention. OpenAI's [voice dictation FAQ](https://help.openai.com/en/articles/12168547-voice-dictation-faq) describes ChatGPT's general data controls, which may or may not apply here.
- WispX reads `%USERPROFILE%\.codex\auth.json`. If the token has expired, it refreshes it and writes the new one back so Codex stays signed in.
- It can't type into apps running as administrator.

## Build

Needs the .NET 9 SDK. From the root of the repository:

```
dotnet publish windows/src -c Release
```

The exe lands in `windows/src/bin/Release/net9.0-windows/win-x64/publish/`.

GitHub Actions builds every push and pull request; the exe is attached to each run under **Artifacts**. Pushing a tag like `v1.0.1` builds it and publishes a release with `WispX.exe` attached.

## Layout

```
windows/src/
  App.xaml.cs         startup, tray, dictation flow
  Audio/              mic recording, sounds
  Input/              Ctrl+Win hook, pasting, Win32 calls
  Transcription/      ChatGPT transcribe + Codex login
  UI/                 the overlay pill
  Setup/              self-install, settings
  Assets/             icon
```
