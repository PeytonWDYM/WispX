using System;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Animation;
using System.Windows.Shapes;
using System.Windows.Threading;
using static WispX.Native;

namespace WispX
{
    public enum MsgKind { Info, Warn, Error, Success }

    /// <summary>
    /// The floating pill. Click-through, never takes focus, never shows in Alt+Tab,
    /// stays topmost, follows the monitor you're working on, and hides its idle
    /// state during fullscreen video/games/presentations.
    /// </summary>
    public partial class Overlay : Window
    {
        private enum Mode { Idle, Recording, Transcribing, Message }

        private const int N = 15;
        private const double BarW = 3, Gap = 3, MaxH = 24, MinH = 3;
        private readonly Rectangle[] _bars = new Rectangle[N];
        private readonly double[] _h = new double[N];
        private readonly double[] _seed = new double[N];

        private Mode _mode = Mode.Idle;
        private Func<float> _level;
        private double _smooth, _t;
        private TimeSpan _last;
        private bool _rendering;
        private IntPtr _hwnd;
        private IntPtr _lastMonitor;
        private readonly DispatcherTimer _msgTimer = new() { Interval = TimeSpan.FromSeconds(2.6) };
        private readonly DispatcherTimer _housekeeping = new() { Interval = TimeSpan.FromSeconds(1.5) };
        private readonly TranslateTransform _glowShift = new(-1, 0);

        internal bool IdlePillEnabled { get; set; } = true;

        public Overlay()
        {
            InitializeComponent();
            var rng = new Random(7);
            for (int i = 0; i < N; i++)
            {
                var r = new Rectangle { Width = BarW, Height = MinH, RadiusX = 1.5, RadiusY = 1.5, Fill = Brushes.White };
                Canvas.SetLeft(r, i * (BarW + Gap));
                Canvas.SetTop(r, (26 - MinH) / 2);
                Bars.Children.Add(r);
                _bars[i] = r;
                _h[i] = MinH;
                _seed[i] = rng.NextDouble() * Math.PI * 2;
            }
            ((LinearGradientBrush)Glow.Background).RelativeTransform = _glowShift;

            _msgTimer.Tick += (s, e) => { _msgTimer.Stop(); Idle(); };
            _housekeeping.Tick += (s, e) => Housekeeping();
            SourceInitialized += (s, e) =>
            {
                _hwnd = new WindowInteropHelper(this).Handle;
                long ex = GetWindowLongPtr(_hwnd, GWL_EXSTYLE).ToInt64();
                ex |= WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TOPMOST;
                ex &= ~WS_EX_APPWINDOW;
                SetWindowLongPtr(_hwnd, GWL_EXSTYLE, new IntPtr(ex));
                HwndSource.FromHwnd(_hwnd)?.AddHook(WndProc);
            };
            Loaded += (s, e) => { Reposition(force: true); _housekeeping.Start(); Idle(); };
        }

        private IntPtr WndProc(IntPtr hwnd, int msg, IntPtr w, IntPtr l, ref bool handled)
        {
            const int WM_MOUSEACTIVATE = 0x21, MA_NOACTIVATE = 3;
            if (msg == WM_MOUSEACTIVATE) { handled = true; return new IntPtr(MA_NOACTIVATE); }
            return IntPtr.Zero;
        }

        // ------------------------------------------------------------------ states

        public void Recording(Func<float> level)
        {
            _msgTimer.Stop();
            Reposition(force: true);
            _mode = Mode.Recording;
            _level = level;
            StartRender();
            Morph(156, 38, 1, Spring());
            Fade(Bars, 1, 160); Fade(Msg, 0, 100); Fade(LockIcon, 0, 120); Fade(RecDot, 0, 120); Fade(Glow, 0, 150);
            Fade(Shadow, 0.5, 200, System.Windows.Media.Effects.DropShadowEffect.OpacityProperty);
            Pop(1.06, 1);
        }

        public void SetLocked()
        {
            if (_mode != Mode.Recording) return;
            Morph(176, 38, 1, Spring());
            Fade(LockIcon, 1, 180);
            Fade(RecDot, 1, 180);
            var pulse = new DoubleAnimation(1, 0.25, TimeSpan.FromMilliseconds(700)) { AutoReverse = true, RepeatBehavior = RepeatBehavior.Forever, BeginTime = TimeSpan.FromMilliseconds(200), EasingFunction = new SineEase { EasingMode = EasingMode.EaseInOut } };
            RecDot.BeginAnimation(OpacityProperty, pulse, HandoffBehavior.Compose);
            Pop(1.04, 1);
        }

        public void Transcribing()
        {
            _msgTimer.Stop();
            _mode = Mode.Transcribing;
            _level = null;
            StartRender();
            Morph(120, 38, 1, Smooth(260));
            Fade(LockIcon, 0, 120); Fade(RecDot, 0, 120); Fade(Msg, 0, 100); Fade(Bars, 1, 150);
            Fade(Glow, 1, 300);
            var sweep = new DoubleAnimation(-1, 1, TimeSpan.FromMilliseconds(1100)) { RepeatBehavior = RepeatBehavior.Forever, EasingFunction = new SineEase { EasingMode = EasingMode.EaseInOut } };
            _glowShift.BeginAnimation(TranslateTransform.XProperty, sweep);
        }

        public bool IsBusyRecording => _mode == Mode.Recording;

        public void Done()
        {
            _mode = Mode.Idle;
            Fade(Glow, 0, 150);
            Pop(1.1, 1, 140);
            var t = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(170) };
            t.Tick += (s, e) => { t.Stop(); if (_mode == Mode.Idle) Idle(); };
            t.Start();
        }

        public void Message(string text, MsgKind kind = MsgKind.Info, double seconds = 2.6)
        {
            if (_mode == Mode.Idle) Reposition(force: true);
            _mode = Mode.Message;
            Msg.Text = text;
            Msg.Foreground = new SolidColorBrush(kind switch
            {
                MsgKind.Error => Color.FromRgb(0xFF, 0xA8, 0xA8),
                MsgKind.Warn => Color.FromRgb(0xFF, 0xDE, 0x9E),
                MsgKind.Success => Color.FromRgb(0xA8, 0xF0, 0xC0),
                _ => Colors.White,
            });
            Msg.Measure(new System.Windows.Size(double.PositiveInfinity, double.PositiveInfinity));
            double w = Math.Min(470, Msg.DesiredSize.Width + 42);
            Morph(w, 40, 1, Spring(0.3));
            Fade(Bars, 0, 100); Fade(LockIcon, 0, 100); Fade(RecDot, 0, 100); Fade(Glow, 0, 150);
            Fade(Msg, 1, 220, delayMs: 90);
            Fade(Shadow, 0.5, 200, System.Windows.Media.Effects.DropShadowEffect.OpacityProperty);
            _msgTimer.Interval = TimeSpan.FromSeconds(seconds);
            _msgTimer.Stop(); _msgTimer.Start();
        }

        public void Idle()
        {
            _msgTimer.Stop();
            _mode = Mode.Idle;
            _level = null;
            Fade(Bars, 0, 120); Fade(Msg, 0, 120); Fade(LockIcon, 0, 120); Fade(RecDot, 0, 120); Fade(Glow, 0, 150);
            Fade(Shadow, 0, 250, System.Windows.Media.Effects.DropShadowEffect.OpacityProperty);
            Morph(44, 8, IdleOpacity(), Smooth(380));
            var t = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(450) };
            t.Tick += (s, e) => { t.Stop(); if (_mode == Mode.Idle) StopRender(); };
            t.Start();
        }

        // ------------------------------------------------------------------ animation helpers

        private double IdleOpacity() => IdlePillEnabled && !SomethingIsFullscreen() ? 0.55 : 0;

        private static IEasingFunction Spring(double amp = 0.45) => new BackEase { Amplitude = amp, EasingMode = EasingMode.EaseOut };
        private static IEasingFunction Smooth(int _ = 0) => new CubicEase { EasingMode = EasingMode.EaseInOut };

        private void Morph(double w, double h, double opacity, IEasingFunction ease)
        {
            int ms = ease is BackEase ? 380 : 300;
            Pill.BeginAnimation(WidthProperty, new DoubleAnimation(w, TimeSpan.FromMilliseconds(ms)) { EasingFunction = ease }, HandoffBehavior.SnapshotAndReplace);
            Pill.BeginAnimation(HeightProperty, new DoubleAnimation(h, TimeSpan.FromMilliseconds(ms)) { EasingFunction = ease }, HandoffBehavior.SnapshotAndReplace);
            Pill.BeginAnimation(OpacityProperty, new DoubleAnimation(opacity, TimeSpan.FromMilliseconds(220)) { EasingFunction = new QuadraticEase() }, HandoffBehavior.SnapshotAndReplace);
        }

        private static void Fade(IAnimatable el, double to, int ms, DependencyProperty dp = null, int delayMs = 0)
        {
            var a = new DoubleAnimation(to, TimeSpan.FromMilliseconds(ms))
            {
                EasingFunction = new QuadraticEase { EasingMode = EasingMode.EaseOut },
                BeginTime = TimeSpan.FromMilliseconds(delayMs),
            };
            el.BeginAnimation(dp ?? UIElement.OpacityProperty, a, HandoffBehavior.SnapshotAndReplace);
        }

        private void Pop(double peak, double end, int ms = 160)
        {
            var a = new DoubleAnimationUsingKeyFrames();
            a.KeyFrames.Add(new EasingDoubleKeyFrame(peak, KeyTime.FromTimeSpan(TimeSpan.FromMilliseconds(ms * 0.45)), new QuadraticEase { EasingMode = EasingMode.EaseOut }));
            a.KeyFrames.Add(new EasingDoubleKeyFrame(end, KeyTime.FromTimeSpan(TimeSpan.FromMilliseconds(ms * 1.6)), new BackEase { Amplitude = 0.6, EasingMode = EasingMode.EaseOut }));
            PillScale.BeginAnimation(ScaleTransform.ScaleXProperty, a, HandoffBehavior.SnapshotAndReplace);
            PillScale.BeginAnimation(ScaleTransform.ScaleYProperty, a, HandoffBehavior.SnapshotAndReplace);
        }

        // ------------------------------------------------------------------ waveform (60fps while visible, off when idle)

        private void StartRender()
        {
            if (_rendering) return;
            _rendering = true;
            _last = TimeSpan.Zero;
            CompositionTarget.Rendering += OnFrame;
        }

        private void StopRender()
        {
            if (!_rendering) return;
            _rendering = false;
            CompositionTarget.Rendering -= OnFrame;
            _glowShift.BeginAnimation(TranslateTransform.XProperty, null);
        }

        private void OnFrame(object sender, EventArgs e)
        {
            var now = ((RenderingEventArgs)e).RenderingTime;
            if (now == _last) return;
            double dt = _last == TimeSpan.Zero ? 1 / 60.0 : Math.Min(0.05, (now - _last).TotalSeconds);
            _last = now;
            _t += dt;

            float lv = _level?.Invoke() ?? 0;
            double k = lv > _smooth ? 22 : 9; // fast attack, slower release, like a real meter
            _smooth += (lv - _smooth) * Math.Min(1, dt * k);

            double mid = (N - 1) / 2.0;
            for (int i = 0; i < N; i++)
            {
                double center = 1 - Math.Abs(i - mid) / mid;
                double target, alpha = 1;
                switch (_mode)
                {
                    case Mode.Recording:
                        double env = 0.28 + 0.72 * Math.Pow(center, 0.9);
                        double wob = 0.55 + 0.45 * Math.Sin(_t * (5.5 + (i % 4) * 1.3) + _seed[i]);
                        double breathe = 1.2 * (0.5 + 0.5 * Math.Sin(_t * 2.4 + i * 0.5));
                        target = MinH + breathe + (MaxH - MinH) * _smooth * env * wob;
                        alpha = 0.55 + 0.45 * Math.Min(1, _smooth * 2 + center * 0.4);
                        break;
                    case Mode.Transcribing:
                        double wave = 0.5 + 0.5 * Math.Sin(_t * 7.5 - i * 0.62);
                        target = MinH + 7 * wave * wave;
                        alpha = 0.35 + 0.65 * wave;
                        break;
                    default:
                        target = MinH; alpha = 0.6;
                        break;
                }
                _h[i] += (target - _h[i]) * Math.Min(1, dt * 18);
                var r = _bars[i];
                r.Height = _h[i];
                Canvas.SetTop(r, (26 - _h[i]) / 2);
                r.Opacity = alpha;
            }
        }

        // ------------------------------------------------------------------ placement & staying on top

        private void Housekeeping()
        {
            if (_hwnd == IntPtr.Zero) return;
            // Other "always on top" windows can climb above us; quietly re-assert.
            SetWindowPos(_hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            if (_mode == Mode.Idle)
            {
                Reposition(force: false);
                double want = IdleOpacity();
                if (Math.Abs(Pill.Opacity - want) > 0.01)
                    Pill.BeginAnimation(OpacityProperty, new DoubleAnimation(want, TimeSpan.FromMilliseconds(300)), HandoffBehavior.SnapshotAndReplace);
            }
        }

        /// <summary>Bottom-center of the work area on the monitor you're actually using.</summary>
        public void Reposition(bool force)
        {
            if (_hwnd == IntPtr.Zero) return;
            var fg = GetForegroundWindow();
            IntPtr mon = fg != IntPtr.Zero && fg != _hwnd ? MonitorFromWindow(fg, 2) : IntPtr.Zero;
            if (mon == IntPtr.Zero && GetCursorPos(out var pt)) mon = MonitorFromPoint(pt, 2);
            if (mon == IntPtr.Zero) return;
            if (!force && mon == _lastMonitor) return;
            _lastMonitor = mon;

            var mi = new MONITORINFO { cbSize = System.Runtime.InteropServices.Marshal.SizeOf<MONITORINFO>() };
            if (!GetMonitorInfo(mon, ref mi)) return;
            double scale = 1;
            try { if (GetDpiForMonitor(mon, 0, out uint dx, out _) == 0) scale = dx / 96.0; } catch { }

            int w = (int)Math.Round(480 * scale), h = (int)Math.Round(120 * scale);
            var wa = mi.rcWork;
            int x = wa.Left + (wa.Right - wa.Left - w) / 2;
            int y = wa.Bottom - h + (int)Math.Round(12 * scale);
            SetWindowPos(_hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            // Moving across monitors with different scaling makes WPF resize us; snap once more after it settles.
            Dispatcher.BeginInvoke(DispatcherPriority.Background, new Action(() =>
                SetWindowPos(_hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE)));
        }
    }
}
