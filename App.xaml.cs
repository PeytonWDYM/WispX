using System;
using System.Diagnostics;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Threading;
using Forms = System.Windows.Forms;

namespace WispX
{
    public partial class App : Application
    {
        public static bool Quitting { get; private set; }

        private const double MinHoldSeconds = 0.3;
        private static readonly TimeSpan MaxRecording = TimeSpan.FromMinutes(10);

        private Mutex _mutex;
        private EventWaitHandle _wake;
        private Settings _settings;
        private Overlay _overlay;
        private Recorder _rec;
        private ChatGptTranscriber _gpt;
        private HotkeyHook _hook;
        private Forms.NotifyIcon _tray;
        private Forms.ToolStripMenuItem _copyLast, _status;

        private bool _recording;
        private DateTime _recStart;
        private string _lastTranscript;
        private int _inFlight;
        private readonly DispatcherTimer _maxTimer = new() { Interval = MaxRecording };

        protected override void OnStartup(StartupEventArgs e)
        {
            base.OnStartup(e);

            int ex0 = Array.IndexOf(e.Args, "--export-sounds");
            if (ex0 >= 0 && e.Args.Length > ex0 + 1) { Sounds.Export(e.Args[ex0 + 1]); Quitting = true; Shutdown(); return; }

            // Running the downloaded WispX.exe installs (or updates) it and relaunches from the install folder.
            if (Installer.EnsureInstalled(e.Args)) { Quitting = true; Shutdown(); return; }

            // One copy only. Opening WispX again from Search just pings the running one.
            _mutex = new Mutex(true, @"Local\WispX.SingleInstance.v1", out bool first);
            if (!first)
            {
                try { EventWaitHandle.OpenExisting(@"Local\WispX.Wake.v1").Set(); } catch { }
                Quitting = true;
                Shutdown();
                return;
            }
            _wake = new EventWaitHandle(false, EventResetMode.AutoReset, @"Local\WispX.Wake.v1");
            new Thread(() => { while (_wake.WaitOne()) Dispatcher.BeginInvoke(() => Greet(again: true)); })
            { IsBackground = true, Name = "WispX.Wake" }.Start();
            var quitEvt = new EventWaitHandle(false, EventResetMode.AutoReset, Installer.QuitEventName);
            new Thread(() => { if (quitEvt.WaitOne()) Dispatcher.BeginInvoke(Quit); })
            { IsBackground = true, Name = "WispX.Quit" }.Start();

            DispatcherUnhandledException += (s, ex) => { ex.Handled = true; try { _overlay?.Message("Something went wrong", MsgKind.Error); } catch { } };

            _settings = Settings.Load();
            Sounds.Enabled = _settings.Sounds;

            _overlay = new Overlay { IdlePillEnabled = _settings.IdlePill };
            _overlay.Show();

            _rec = new Recorder();
            _gpt = new ChatGptTranscriber();
            ChatGptTranscriber.Warmup();

            _hook = new HotkeyHook(ev => Dispatcher.BeginInvoke(() => OnHotkey(ev)));
            _hook.Start();

            _maxTimer.Tick += async (s, ev) => { _maxTimer.Stop(); _hook.ForceIdle(); await FinishAsync(); };

            BuildTray();

            int st = Array.FindIndex(e.Args, a => a == "--selftest");
            if (st >= 0 && e.Args.Length > st + 2) { _ = SelfTestAsync(e.Args[st + 1], e.Args[st + 2]); return; }

            bool background = e.Args.Any(a => a.Equals("--background", StringComparison.OrdinalIgnoreCase));
            if (!background) Dispatcher.BeginInvoke(DispatcherPriority.ApplicationIdle, () => Greet(again: false));
            if (!_settings.FirstRunDone)
            {
                _settings.FirstRunDone = true;
                _settings.Save();
                _tray.ShowBalloonTip(6000, "WispX is running",
                    "Hold Ctrl + Win and talk. Let go to type it.\nCtrl + Win + Space = hands-free. Esc = cancel.", Forms.ToolTipIcon.None);
            }
        }

        private void Greet(bool again)
        {
            if (!ChatGptTranscriber.HasLogin())
                _overlay.Message("Sign in to the Codex app first", MsgKind.Warn, 4);
            else
                _overlay.Message(again ? "WispX is running · hold Ctrl + Win" : "Hold Ctrl + Win to dictate", MsgKind.Info, 2.8);
        }

        // ------------------------------------------------------------------ dictation flow

        private void OnHotkey(HotkeyEvent ev)
        {
            switch (ev)
            {
                case HotkeyEvent.Start: StartRecording(); break;
                case HotkeyEvent.Lock: if (_recording) _overlay.SetLocked(); break;
                case HotkeyEvent.Release:
                case HotkeyEvent.StopLocked: _ = FinishAsync(); break;
                case HotkeyEvent.CancelSilent: CancelRecording(sound: false); break;
                case HotkeyEvent.Cancel: CancelRecording(sound: true); break;
            }
        }

        private void StartRecording()
        {
            if (_recording) return;
            if (!ChatGptTranscriber.HasLogin())
            {
                _hook.ForceIdle();
                _overlay.Message("Sign in to the Codex app first", MsgKind.Warn, 3.5);
                return;
            }
            try { _rec.Start(); }
            catch
            {
                _hook.ForceIdle();
                _overlay.Message("No microphone found", MsgKind.Error);
                return;
            }
            _recording = true;
            _recStart = DateTime.UtcNow;
            _overlay.Recording(() => _rec.Level);
            Sounds.Start();
            _maxTimer.Stop(); _maxTimer.Start();
        }

        private void CancelRecording(bool sound)
        {
            if (!_recording) return;
            _recording = false;
            _maxTimer.Stop();
            _rec.Cancel();
            if (sound) Sounds.Cancel();
            if (_inFlight == 0) _overlay.Idle(); else _overlay.Transcribing();
        }

        private async Task FinishAsync()
        {
            if (!_recording) return;
            _recording = false;
            _maxTimer.Stop();
            var held = DateTime.UtcNow - _recStart;
            IntPtr target = Native.GetForegroundWindow();

            // Results land after an await; if you already started a new recording, leave its pill alone.
            void Ui(Action show) { if (!_recording) show(); }

            if (held.TotalSeconds < MinHoldSeconds)
            {
                // A quick tap is almost always an accident. Back off quietly.
                _rec.Cancel();
                if (_inFlight == 0) _overlay.Idle();
                return;
            }
            if (_rec.Peak < 0.12f)
            {
                _rec.Cancel();
                Sounds.Stop();
                _overlay.Message("Didn't hear anything. Check your mic?", MsgKind.Warn);
                return;
            }

            // Switch to "loading" the instant you let go; the mic finishes shutting off behind it.
            _inFlight++;
            Sounds.Stop();
            _overlay.Transcribing();
            try
            {
                var (wav, peak, ms) = await _rec.StopAsync();
                if (wav == null) { Ui(() => _overlay.Idle()); return; }
                string text = (await _gpt.TranscribeAsync(wav, ms, _settings.Language)).Trim();
                if (string.IsNullOrWhiteSpace(text))
                {
                    Ui(() => _overlay.Message("Didn't catch that", MsgKind.Warn));
                    return;
                }
                _lastTranscript = text;
                _copyLast.Enabled = true;
                bool pasted = await Injector.PasteAsync(text, target);
                if (pasted) Ui(() => _overlay.Done());
                else Ui(() => _overlay.Message("Copied. You switched windows, so press Ctrl + V", MsgKind.Info, 3.5));
            }
            catch (TranscribeException ex)
            {
                Ui(() => _overlay.Message(ex.Message, MsgKind.Error, 3.5));
            }
            catch (Exception)
            {
                Ui(() => _overlay.Message("Transcription failed", MsgKind.Error));
            }
            finally
            {
                _inFlight--;
            }
        }

        /// <summary>Dev check: transcribes a WAV through the real pipeline and walks the overlay through each state.</summary>
        private async Task SelfTestAsync(string wavPath, string outPath)
        {
            var log = new System.Text.StringBuilder();
            void Mark(string s) { log.AppendLine($"{DateTime.Now:HH:mm:ss.fff} {s}"); System.IO.File.WriteAllText(outPath, log.ToString()); }
            await Task.Delay(1500);
            Mark("idle");
            await Task.Delay(1500);
            var t0 = DateTime.UtcNow;
            _overlay.Recording(() => (float)(0.55 + 0.45 * Math.Sin((DateTime.UtcNow - t0).TotalSeconds * 3)));
            Mark("recording"); await Task.Delay(2000);
            _overlay.SetLocked(); Mark("locked"); await Task.Delay(2000);
            _overlay.Transcribing(); Mark("transcribing");
            var sw = Stopwatch.StartNew();
            try
            {
                var bytes = System.IO.File.ReadAllBytes(wavPath);
                var text = await _gpt.TranscribeAsync(bytes, (bytes.Length - 44) / 32, _settings.Language);
                Mark($"TRANSCRIPT ({sw.ElapsedMilliseconds} ms): {text}");
            }
            catch (Exception ex) { Mark("ERROR: " + ex.Message); }
            await Task.Delay(1200);
            _overlay.Done(); Mark("done"); await Task.Delay(1500);
            _overlay.Message("Copied. You switched windows, so press Ctrl + V", MsgKind.Info, 3); Mark("message"); await Task.Delay(2500);
            _overlay.Message("Sign in to the Codex app first", MsgKind.Warn, 3); Mark("warn"); await Task.Delay(2500);
            Mark("end");
            Quit();
        }

        // ------------------------------------------------------------------ tray

        private void BuildTray()
        {
            System.Drawing.Icon icon;
            try { icon = System.Drawing.Icon.ExtractAssociatedIcon(Environment.ProcessPath) ?? System.Drawing.SystemIcons.Application; }
            catch { icon = System.Drawing.SystemIcons.Application; }

            var menu = new Forms.ContextMenuStrip { ShowImageMargin = false, ShowCheckMargin = true };
            var title = new Forms.ToolStripMenuItem("WispX · hold Ctrl + Win") { Enabled = false };
            _status = new Forms.ToolStripMenuItem("") { Enabled = false };
            _copyLast = new Forms.ToolStripMenuItem("Copy last transcript", null, (s, e) =>
            {
                if (_lastTranscript != null) Injector.SetText(_lastTranscript, excludeFromHistory: false);
            }) { Enabled = false };

            var sounds = new Forms.ToolStripMenuItem("Sounds") { Checked = _settings.Sounds, CheckOnClick = true };
            sounds.CheckedChanged += (s, e) => { _settings.Sounds = Sounds.Enabled = sounds.Checked; _settings.Save(); };

            var pill = new Forms.ToolStripMenuItem("Show idle pill") { Checked = _settings.IdlePill, CheckOnClick = true };
            pill.CheckedChanged += (s, e) => { _settings.IdlePill = _overlay.IdlePillEnabled = pill.Checked; _settings.Save(); _overlay.Idle(); };

            var startup = new Forms.ToolStripMenuItem("Start with Windows") { CheckOnClick = true };
            try { startup.Checked = Settings.StartWithWindows; } catch { }
            startup.CheckedChanged += (s, e) => { try { Settings.StartWithWindows = startup.Checked; } catch { } };

            var help = new Forms.ToolStripMenuItem("How to use", null, (s, e) =>
                _tray.ShowBalloonTip(7000, "WispX",
                    "Hold Ctrl + Win → talk → let go. Text is typed where your cursor is.\n" +
                    "Ctrl + Win, then Space → hands-free. Ctrl + Win again to finish.\nEsc → cancel.",
                    Forms.ToolTipIcon.None));

            var quit = new Forms.ToolStripMenuItem("Quit WispX", null, (s, e) => Quit());
            var uninstall = new Forms.ToolStripMenuItem("Uninstall WispX…", null, (s, e) =>
            {
                var ok = Forms.MessageBox.Show("Remove WispX from this PC?\n\nYour Codex/ChatGPT login isn't touched.",
                    "Uninstall WispX", Forms.MessageBoxButtons.OKCancel, Forms.MessageBoxIcon.None);
                if (ok != Forms.DialogResult.OK) return;
                Installer.Uninstall();
                Quit();
            });

            menu.Items.AddRange(new Forms.ToolStripItem[]
            {
                title, _status, new Forms.ToolStripSeparator(),
                _copyLast, new Forms.ToolStripSeparator(),
                sounds, pill, startup, new Forms.ToolStripSeparator(),
                help, uninstall, quit,
            });
            menu.Opening += (s, e) => _status.Text = ChatGptTranscriber.HasLogin()
                ? "✓ Using your ChatGPT login from Codex"
                : "⚠ Not signed in. Open Codex and sign in";

            _tray = new Forms.NotifyIcon { Icon = icon, Text = "WispX · hold Ctrl + Win to dictate", ContextMenuStrip = menu, Visible = true };
            _tray.MouseClick += (s, e) => { if (e.Button == Forms.MouseButtons.Left) Greet(again: true); };
        }

        private void Quit()
        {
            Quitting = true;
            try { _rec?.Cancel(); } catch { }
            try { _hook?.Dispose(); } catch { }
            if (_tray != null) { _tray.Visible = false; _tray.Dispose(); }
            Shutdown();
        }

        protected override void OnExit(ExitEventArgs e)
        {
            if (_tray != null) { _tray.Visible = false; _tray.Dispose(); }
            base.OnExit(e);
        }
    }
}
