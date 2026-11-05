# WispX

[![CI](https://github.com/PeytonWDYM/WispX/actions/workflows/ci.yml/badge.svg)](https://github.com/PeytonWDYM/WispX/actions/workflows/ci.yml)

Hold a key, talk, let go. What you said appears where your cursor is.

This is the Linux version of the WispX app, written in Rust and built first for **KDE Plasma on Wayland**. A small pill sits at the bottom of your screen and shows what WispX is doing, and a tray icon holds the rest. Speech is recognised on your own machine by [whisper.cpp](https://github.com/ggml-org/whisper.cpp), so by default no audio leaves your computer. It needs no root access and never reads your keyboard: Plasma owns the shortcuts and tells WispX when they go down and come up.

Looking for the Windows app? See [windows/README.md](windows/README.md).

## Quick start on KDE Plasma

You need Rust 1.89 or newer, a C++ compiler, Qt 6 and LayerShellQt to build it, and [whisper.cpp](https://github.com/ggml-org/whisper.cpp) with `whisper-cli` on your `PATH`, `wl-clipboard`, and PipeWire to run it. Plasma 6 includes the rest.

```sh
# Arch and CachyOS: whisper.cpp itself comes from the AUR
sudo pacman -S --needed base-devel rust qt6-base qt6-declarative qt6-wayland layer-shell-qt wl-clipboard

# Debian and Ubuntu, on a release with Plasma 6 and Qt 6. These names aren't tested
sudo apt install build-essential cargo pkg-config qt6-base-dev qt6-base-private-dev qt6-declarative-dev \
  qt6-wayland liblayershellqtinterface-dev libwayland-dev qml6-module-qtquick qml6-module-qtquick-controls \
  qml6-module-qtquick-layouts qml6-module-qtquick-dialogs qml6-module-qtquick-window wl-clipboard
```

```sh
cargo build --release
install -Dm755 target/release/wispx target/release/wispx-gui -t ~/.local/bin/
wispx
```

That opens WispX. From then on it's also in your applications menu, and the terminal can close: the app keeps running in the background. Opening it a second time brings up its settings window.

The first time, the settings window lists what's left to do, with a button for each:

- **Speech model:** download one, or choose one you already have. A model already at `~/.local/share/wispx/models/ggml-base.bin` is used as it is.
- **Shortcuts:** Plasma asks you to confirm them. After that, they just work.
- **Automatic paste:** press **Enable automatic paste** and allow it in the dialog.

| Keys | What it does |
| --- | --- |
| Hold Ctrl + Meta + D | Records. Let go and your words are delivered. |
| Ctrl + Meta + Space | Hands-free: starts recording, or keeps a hold going after you let go. Press it again to finish. |
| Ctrl + Meta + Backspace | Cancels the recording or the transcription. |

To change the keys, use System Settings > Keyboard > Shortcuts > WispX, and the settings window shows whatever is really bound. If a shortcut does nothing, check there whether something else uses the same keys: Plasma can report a shortcut as set even when another one takes it.

### What you'll see

The pill never takes focus, so your cursor stays where you were typing:

| Pill | Meaning |
| --- | --- |
| A small dark bar | Ready. You can hide this with **Show idle pill**. |
| White bars that move with your voice | Recording. They follow the real microphone level. |
| A lock and a pulsing red dot around the bars | Hands-free. |
| A wave travelling across | Transcribing. |
| A short message | What just happened, such as a failure or text left on the clipboard. |

The tray icon's menu has the status, **Copy last transcript**, **Sounds**, **Show idle pill**, **Start at login**, **Settings**, **How to use** and **Quit**. The settings window also chooses the speech backend, model, language and microphone. Quitting stops the background service and the recording with it. If you started `wispx daemon` yourself, the app uses that one, and leaves it running when it quits.

### Pasting automatically

Until you allow it, your words go to the clipboard and you press Ctrl + V. **Enable automatic paste** in the settings window asks the desktop once, and it remembers. `wispx paste-setup` does the same from a terminal.

This uses the RemoteDesktop portal, asking for the keyboard only, because that's the sanctioned way for a Wayland program to press keys. Plasma may show that a remote control session is active while WispX runs. To stop WispX using it, delete `~/.local/share/wispx/remote-desktop-token` and quit and reopen WispX.

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
wispx                  Open the app, or bring up the one already running
wispx app --background Open it with no window unless something needs attention
wispx daemon           Run without any window, in the foreground, and listen for shortcuts
wispx doctor           Check tools, models and desktop support; changes nothing
wispx status [--json]  Show what WispX is doing
wispx last             Print the last transcript
wispx copy-last        Copy the last transcript again
wispx cancel           Throw away the recording or running transcription
wispx backend [NAME]   Show or change the backend: local or chatgpt
wispx transcribe FILE  Print the text of a 16 kHz mono 16-bit WAV file
```

`wispx --help` lists everything. `wispx daemon` needs no display and no Qt, and shows feedback as desktop notifications instead of the pill. The `transcribe` command needs no daemon, and prints only the transcript, so it works in scripts. Convert other audio with `ffmpeg -i in.mp3 -ar 16000 -ac 1 -c:a pcm_s16le out.wav`.

## Other desktops

Plasma is the only desktop WispX has been tested on with its full workflow. Elsewhere, `wispx doctor` tells you what your session offers.

| Session | Shortcuts | Delivery | Pill |
| --- | --- | --- | --- |
| KDE Plasma on Wayland | Portal shortcuts, including hold and release | Automatic paste, or clipboard | Yes |
| GNOME on Wayland | Portal shortcuts if your version offers them; not tested | Clipboard | No: GNOME has no layer shell, so feedback falls back to notifications |
| Other Wayland desktops | Your desktop's own key bindings, see below | Clipboard | If the compositor has a layer shell; not tested |
| X11 (needs `xclip` or `xsel`) | Your window manager's key bindings, see below | Clipboard | Yes; not tested |

Automatic paste needs a way to verify the focused window, and WispX only has one on Plasma. No Wayland program can reliably do this everywhere, so WispX falls back to the clipboard rather than risk typing into the wrong window.

Without portal shortcuts, bind these commands in your desktop:

```
wispx press      on key down
wispx release    on key up
wispx toggle     hands-free, for desktops that can only run a command on key down
wispx cancel     to cancel
```

On Sway, for example, set `"hotkey": "none"` in WispX's settings and add this to your Sway config. It follows the sway(5) man page, but WispX hasn't been tested on Sway:

```
bindsym --no-repeat $mod+t exec wispx press
bindsym --release $mod+t exec wispx release
bindsym --no-repeat $mod+Shift+t exec wispx toggle
bindsym --no-repeat $mod+BackSpace exec wispx cancel
```

## Settings

The settings window covers what most people change, and applies it straight away. Everything else is in `~/.config/wispx/config.json`: `wispx config init` writes the file, and `wispx reload` applies changes. Unknown settings are rejected, so typos don't silently do nothing.

| Setting | Default | Meaning |
| --- | --- | --- |
| `backend` | `local` | `local` or `chatgpt`. WispX never falls back from one to the other. |
| `language` | auto-detect | Language code such as `en`. |
| `sounds` | `true` | Sounds for start, stop and errors. |
| `idle_pill` | `true` | Keep a small pill on screen while WispX is ready. |
| `notifications` | `all` | Popups: `all`, `errors` or `none`. The app's pill takes their place while it's open. |
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

You can use ChatGPT for transcription instead, with the login the Codex app or CLI already saved. There's no API key and no extra sign-in. It's off unless you turn it on, under **Speech** in the settings window, which asks you to confirm, or with:

```sh
wispx backend chatgpt
```

Before you do:

- **Your audio is uploaded to chatgpt.com.** How long this private endpoint keeps it isn't publicly documented, so WispX can't promise anything about retention. OpenAI's [voice dictation FAQ](https://help.openai.com/en/articles/12168547-voice-dictation-faq) describes ChatGPT's general data controls, which may or may not apply here.
- It calls ChatGPT's `/backend-api/transcribe` endpoint. It's not a public API, so it can change or break without warning, and using it may go against OpenAI's terms.
- WispX only reads `~/.codex/auth.json`. It never refreshes the login or writes to that file, so it can't interfere with Codex. If the login has expired, opening Codex renews it. The Windows app does refresh it, so this is one difference between them.

While the daemon is running, `wispx backend` works on the daemon rather than on a file alone:

- `wispx backend` reports the backend the daemon is actually using, including one chosen with `wispx daemon --backend`.
- `wispx backend NAME` changes the daemon and the config file it was started with, both or neither. It also replaces a `--backend` start flag until the daemon next starts.
- `--config` has to name the daemon's own file. Any other file is refused, so run `wispx quit` first to change one.
- A transcription already under way finishes with the backend it started with. `wispx cancel` stops it.

## What WispX runs

WispX has one direct Rust dependency, `serde_json`, which brings in 4 more crates. The windows are drawn by a separate program, `wispx-gui`, using your system's Qt 6 and LayerShellQt through a small C++ bridge. Everything they show is decided in Rust, and `wispx` itself links neither. To build without them, for a server or a desktop you don't want windows on, use `cargo build --release --no-default-features`.

WispX keeps the rest small by running the tools your system already has, as separate programs with separate arguments and no shell in between:

| Tool | Used for | Needed |
| --- | --- | --- |
| `whisper-cli` | Speech recognition | For the local backend |
| `pw-record`, `parec` or `arecord` | Recording, in that order of preference | Yes, one of them |
| `wl-copy`, `wl-paste` | Clipboard on Wayland | Yes on Wayland |
| `xclip` or `xsel` | Clipboard on X11 | Only on X11 |
| `pw-play`, `paplay` or `aplay` | Sounds | Optional |
| `curl` | Downloading a speech model when you ask; sending audio to ChatGPT, with the token passed over stdin rather than on a command line | For those two things |
| `pactl` | Listing microphones in the settings window | Optional |

It also talks over D-Bus, through a small client built into WispX, to these services on your session bus:

| Service | Used for |
| --- | --- |
| `xdg-desktop-portal` with its KDE backend | Shortcuts and, once allowed, pressing the paste keys |
| KWin | Finding out which window has focus, with a small script it loads and removes for each check |
| A notification server | Popups, for `wispx daemon` |
| The status notifier watcher | The tray icon |

## Differences from the Windows app

Most of these come from Wayland, which deliberately doesn't let programs watch the keyboard or type freely.

| | Windows | Linux |
| --- | --- | --- |
| Transcription | ChatGPT only | whisper.cpp by default, ChatGPT optional |
| Hold key | Ctrl + Win | Ctrl + Meta + D, since Plasma fires shortcuts made only of modifiers on release, which can't work for holding |
| Hands-free | Ctrl + Win, then Space | Ctrl + Meta + Space, alone or during a hold |
| Cancel | Esc | Ctrl + Meta + Backspace, since a program can't claim plain Esc on Wayland |
| Pressing another key during a hold | Cancels | Nothing happens, since WispX can't see other keys |
| Typing into the target | Always | Needs **Enable automatic paste**, otherwise clipboard |
| On-screen feedback | Overlay pill | The same pill, anchored through the Wayland layer shell |
| Controls | Tray icon | Tray icon, a settings window, and `wispx` commands |
| Fullscreen apps | Hides the idle pill | The same: a fullscreen window covers it until it has something to show |
| Install | Self-installing exe | Built from source, and adds itself to the applications menu when opened |
| Codex login | Refreshed and written back | Read only |

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release

# The same without Qt
cargo test --no-default-features
```

`cargo test` needs no microphone, display, network or account. Recorders, `whisper-cli`, `curl` and clipboard tools are scripts on a private `PATH`, and the desktop's services are a mock on a private `dbus-daemon`. Each test gets scratch directories and a cleared environment.

Two further test files run the real desktop stack: a private headless KWin with the real portal, KDE backend, shortcuts, consent dialogs, clipboard tools and a real terminal window. They press real keys inside that session, with only the microphone replaced. `gui_nested` also runs the real app with a real panel and tray, clicks its real buttons, asks the compositor where its windows are and which has focus, and leaves screenshots in `/tmp/wispx-ui-review/`. They start a compositor, so they're opt-in, and they're isolated from whatever session you run them from:

```sh
cargo test --test plasma_nested -- --ignored --nocapture --test-threads=1
cargo test --test gui_nested -- --ignored --nocapture --test-threads=1

# Also with real speech recognition, given a model and a 16 kHz mono WAV
export WISPX_TEST_MODEL=/path/to/ggml-tiny.en.bin WISPX_TEST_WAV=/path/to/jfk.wav
cargo test --test plasma_nested -- --ignored --nocapture --test-threads=1
cargo test --test gui_nested -- --ignored --nocapture --test-threads=1
```

The app draws in software in these tests, so they need no GPU. Set `WISPX_TEST_QT_GPU=1` to run `gui_nested` with the GPU, as on a real desktop.

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
  app/                the app's logic, with no Qt: what the windows show and do
  bin/wispx-gui.rs    the program that draws them
ui/                   the pill and settings in QML, and the C++ bridge to Qt
build.rs              builds the bridge
tests/
  e2e.rs              the real binary against stand-ins
  plasma_nested.rs    the real binary against the real desktop stack
  gui_nested.rs       the real app against the real desktop stack
windows/              the Windows app
packaging/            icons, and desktop entry and systemd unit examples
```
