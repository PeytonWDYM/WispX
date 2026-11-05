# WispX

[![CI](https://github.com/PeytonWDYM/WispX/actions/workflows/ci.yml/badge.svg)](https://github.com/PeytonWDYM/WispX/actions/workflows/ci.yml)

Hold a key, talk, let go. What you said appears where your cursor is.

This is the Linux version, written in Rust and built first for **KDE Plasma on Wayland**. Speech is recognised on your own machine by [whisper.cpp](https://github.com/ggml-org/whisper.cpp), so by default no audio leaves your computer. It needs no root access and never reads your keyboard: Plasma owns the shortcuts and tells WispX when they go down and come up.

Looking for the Windows app? See [windows/README.md](windows/README.md).

## Quick start on KDE Plasma

You need Rust 1.89 or newer, [whisper.cpp](https://github.com/ggml-org/whisper.cpp) with `whisper-cli` on your `PATH`, `wl-clipboard`, and PipeWire. Plasma 6 includes the rest.

```sh
# Build and install
cargo build --release
install -Dm755 target/release/wispx ~/.local/bin/wispx

# Download a speech model to where WispX looks for it
curl -L --create-dirs -o ~/.local/share/wispx/models/ggml-base.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin

# Let the desktop recognise WispX, and check that everything is in place
wispx setup
wispx doctor

# Start it
wispx daemon
```

The first time it starts, Plasma asks you to confirm the shortcuts. After that, they just work:

| Keys | What it does |
| --- | --- |
| Hold Ctrl + Meta + D | Records. Let go and your words are delivered. |
| Ctrl + Meta + Space | Hands-free: starts recording, or keeps a hold going after you let go. Press it again to finish. |
| Ctrl + Meta + Backspace | Cancels the recording or the transcription. |

To start WispX when you log in, run `wispx setup --autostart`. To change the keys, use System Settings > Keyboard > Shortcuts > WispX. If a shortcut does nothing, check there whether something else uses the same keys: Plasma can report a shortcut as set even when another one takes it.

### Pasting automatically

Out of the box, your words go to the clipboard and you press Ctrl + V. To have them pasted for you, run this once and allow it in the dialog:

```sh
wispx paste-setup
```

This uses the RemoteDesktop portal, asking for the keyboard only, because that's the sanctioned way for a Wayland program to press keys. Plasma may show that a remote control session is active while WispX runs. To stop WispX using it, delete `~/.local/share/wispx/remote-desktop-token` and restart the daemon.

## What happens to your words

WispX notes which window has focus when you start talking. Once the text is ready, it checks again:

- **Same window:** it pastes, using Ctrl + Shift + V in terminals and Ctrl + V elsewhere, then puts your previous clipboard text back.
- **Different window, or it can't tell:** it never presses a key. Your words stay on the clipboard for you to paste.

So if you switch windows while talking, the text stays on the clipboard unless you're back in the first window by the time it's ready. The same goes if KWin hasn't said which window has focus by the time you stop talking.

Good to know:

- Only plain text on the clipboard is put back. If you had copied an image or files, the transcript replaces them.
- Pasted text is marked sensitive so clipboard history should skip it. Whether Klipper honours that hasn't been verified. Text left on the clipboard for you isn't marked.
- A recording is kept only while it's being transcribed, in a file only you can read, under `$XDG_RUNTIME_DIR/wispx`. A daemon that crashes removes any leftovers on its next start.
- Transcripts stay in memory for `wispx last` and `wispx copy-last`. They aren't written to disk, or to logs.

## Commands

```
wispx daemon           Run in the foreground and listen for shortcuts
wispx doctor           Check tools, models and desktop support; changes nothing
wispx status [--json]  Show what WispX is doing
wispx last             Print the last transcript
wispx copy-last        Copy the last transcript again
wispx cancel           Throw away the recording or running transcription
wispx backend [NAME]   Show or change the backend: local or chatgpt
wispx transcribe FILE  Print the text of a 16 kHz mono 16-bit WAV file
```

`wispx --help` lists everything. The `transcribe` command needs no daemon, and prints only the transcript, so it works in scripts. Convert other audio with `ffmpeg -i in.mp3 -ar 16000 -ac 1 -c:a pcm_s16le out.wav`.

## Other desktops

Plasma is the only desktop WispX has been tested on with its full workflow. Elsewhere, `wispx doctor` tells you what your session offers.

| Session | Shortcuts | Delivery |
| --- | --- | --- |
| KDE Plasma on Wayland | Portal shortcuts, including hold and release | Automatic paste, or clipboard |
| GNOME on Wayland | Portal shortcuts if your version offers them; not tested | Clipboard |
| Other Wayland desktops | Your desktop's own key bindings, see below | Clipboard |
| X11 (needs `xclip` or `xsel`) | Your window manager's key bindings, see below | Clipboard |

Automatic paste needs a way to verify the focused window, and WispX only has one on Plasma. No Wayland program can reliably do this everywhere, so WispX falls back to the clipboard rather than risk typing into the wrong window.

Without portal shortcuts, bind these commands in your desktop:

```
wispx press      on key down
wispx release    on key up
wispx toggle     hands-free, for desktops that can only run a command on key down
wispx cancel     to cancel
```

## Settings

`wispx config init` writes the settings to `~/.config/wispx/config.json`, and `wispx reload` applies changes. Unknown settings are rejected, so typos don't silently do nothing.

| Setting | Default | Meaning |
| --- | --- | --- |
| `backend` | `local` | `local` or `chatgpt`. WispX never falls back from one to the other. |
| `language` | auto-detect | Language code such as `en`. |
| `sounds` | `true` | Sounds for start, stop and errors. |
| `notifications` | `all` | Popups: `all`, `errors` or `none`. |
| `hotkey` | `auto` | `auto`, `portal` or `none`. |
| `shortcuts` | see above | Keys suggested to the desktop on first start, such as `CTRL+LOGO+d`. Once set, change them in System Settings. |
| `paste` | `auto` | `auto`, `copy`, or `none` to only keep text for `wispx last`. |
| `paste_shortcut` | `auto` | `auto`, `ctrl+v`, `ctrl+shift+v` or `shift+insert`. |
| `restore_clipboard` | `true` | Put your clipboard text back after pasting. |
| `min_hold_ms` | `300` | Shorter holds are treated as accidental taps. |
| `max_recording_secs` | `600` | Recording finishes by itself after this long. |
| `silence_threshold` | `0.12` | Below this level, a recording counts as silence and isn't transcribed. |
| `audio.recorder` | `auto` | `auto`, `pw-record`, `parec` or `arecord`. |
| `audio.device` | system default | Input device to record from. |
| `whisper.binary` | `whisper-cli` | The whisper.cpp program. |
| `whisper.model` | `~/.local/share/wispx/models/ggml-base.bin` | The speech model. |
| `whisper.threads` | `0` | `0` chooses automatically. |
| `whisper.timeout_secs` | `900` | Longest a transcription may run. |
| `whisper.extra_args` | `[]` | Further arguments for `whisper-cli`. |
| `chatgpt.auth_file` | `~/.codex/auth.json` | Where the Codex login is. |
| `chatgpt.timeout_secs` | `120` | Longest a transcription may run. |

## The ChatGPT backend

You can use ChatGPT for transcription instead, with the login the Codex app or CLI already saved. There's no API key and no extra sign-in. It's off unless you turn it on:

```sh
wispx backend chatgpt
```

Before you do:

- **Your audio goes to chatgpt.com.** ChatGPT keeps it for 30 days, the same as the app's own dictation.
- It calls ChatGPT's `/backend-api/transcribe` endpoint. It's not a public API, so it can change or break without warning, and using it may go against OpenAI's terms.
- WispX only reads `~/.codex/auth.json`. It never refreshes the login or writes to that file, so it can't interfere with Codex. If the login has expired, opening Codex renews it. The Windows app does refresh it, so this is one difference between them.

## What WispX runs

WispX has one direct Rust dependency, `serde_json`, which brings in 4 more crates. It keeps that small by running the tools your system already has, as separate programs with separate arguments and no shell in between:

| Tool | Used for | Needed |
| --- | --- | --- |
| `whisper-cli` | Speech recognition | For the local backend |
| `pw-record`, `parec` or `arecord` | Recording, in that order of preference | Yes, one of them |
| `wl-copy`, `wl-paste` | Clipboard on Wayland | Yes on Wayland |
| `xclip` or `xsel` | Clipboard on X11 | Only on X11 |
| `pw-play`, `paplay` or `aplay` | Sounds | Optional |
| `curl` | Sending audio to ChatGPT, with the token passed over stdin rather than on a command line | Only for the ChatGPT backend |

It also talks over D-Bus, through a small client built into WispX, to these services on your session bus:

| Service | Used for |
| --- | --- |
| `xdg-desktop-portal` with its KDE backend | Shortcuts and, once allowed, pressing the paste keys |
| KWin | Finding out which window has focus, with a small script it loads and removes for each check |
| A notification server | Popups |

## Differences from the Windows app

Most of these come from Wayland, which deliberately doesn't let programs watch the keyboard or type freely.

| | Windows | Linux |
| --- | --- | --- |
| Transcription | ChatGPT only | whisper.cpp by default, ChatGPT optional |
| Hold key | Ctrl + Win | Ctrl + Meta + D, since Plasma fires shortcuts made only of modifiers on release, which can't work for holding |
| Hands-free | Ctrl + Win, then Space | Ctrl + Meta + Space, alone or during a hold |
| Cancel | Esc | Ctrl + Meta + Backspace, since a program can't claim plain Esc on Wayland |
| Pressing another key during a hold | Cancels | Nothing happens, since WispX can't see other keys |
| Typing into the target | Always | Needs `wispx paste-setup`, otherwise clipboard |
| On-screen feedback | Overlay pill | Desktop notifications and sounds |
| Controls | Tray icon | `wispx` commands; there's no tray icon |
| Install | Self-installing exe | Built from source, with `wispx setup` for the desktop entry |
| Codex login | Refreshed and written back | Read only |

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`cargo test` needs no microphone, display, network or account. Recorders, `whisper-cli`, `curl` and clipboard tools are scripts on a private `PATH`, and the desktop's services are a mock on a private `dbus-daemon`. Each test gets scratch directories and a cleared environment.

One further test file runs the real desktop stack: a private headless KWin with the real portal, KDE backend, shortcuts, consent dialogs, clipboard tools and a real terminal window. It presses real keys inside that session, with only the microphone replaced. It starts a compositor, so it's opt-in. It's isolated from whatever session you run it from:

```sh
cargo test --test plasma_nested -- --ignored --nocapture --test-threads=1

# Also with real speech recognition, given a model and a 16 kHz mono WAV
WISPX_TEST_MODEL=/path/to/ggml-tiny.en.bin WISPX_TEST_WAV=/path/to/jfk.wav \
  cargo test --test plasma_nested -- --ignored --nocapture --test-threads=1
```

```
src/
  main.rs, cli.rs     command line
  daemon.rs           state, jobs and delivery; everything slow reports back here
  session.rs          the hold, hands-free and cancel state machine
  ipc.rs              the daemon's local socket
  shortcuts.rs        global shortcuts through the portal
  portal.rs, dbus.rs  the D-Bus client and portal requests
  desktop/            clipboard, focus checks, key presses, notifications
  audio/              recording, WAV files, silence detection
  transcribe/         whisper.cpp and ChatGPT
  config.rs, paths.rs settings and private directories
  doctor.rs           `wispx doctor`
tests/
  e2e.rs              the real binary against stand-ins
  plasma_nested.rs    the real binary against the real desktop stack
windows/              the Windows app
packaging/            desktop entry and systemd unit examples
```
