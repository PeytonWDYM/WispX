# WispX

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

The tray icon has: copy last transcript, sounds, idle pill, start with Windows, and uninstall.

## Notes

- WispX calls ChatGPT's `/backend-api/transcribe` endpoint. It's not a public API, so it can change or break without warning, and using it may go against OpenAI's terms.
- ChatGPT keeps the audio for 30 days, the same as the app's own dictation.
- WispX reads `%USERPROFILE%\.codex\auth.json`. If the token has expired, it refreshes it and writes the new one back so Codex stays signed in.
- It can't type into apps running as administrator.

## Build

Needs the .NET 9 SDK.

```
dotnet publish -c Release
```
