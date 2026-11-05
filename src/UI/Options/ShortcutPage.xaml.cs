using System;
using System.Windows;
using System.Windows.Controls;

namespace WispX
{
    /// <summary>Captures physical modifiers through the global hook, then saves a valid chord immediately.</summary>
    public partial class ShortcutPage : UserControl
    {
        private readonly Settings _settings;
        private readonly HotkeyHook _hook;
        private int _captureVersion;
        public bool IsCapturing { get; private set; }

        internal ShortcutPage(Settings settings, HotkeyHook hook)
        {
            InitializeComponent();
            _settings = settings; _hook = hook;
            ShowKeys(false);
            Unloaded += (s, e) => CancelCapture();
        }

        private void ShowKeys(bool animate)
        {
            Keycaps.Children.Clear();
            string[] keys = _settings.Hotkey.Display.Split(" + ");
            for (int i = 0; i < keys.Length; i++)
            {
                if (i > 0) Keycaps.Children.Add(new TextBlock { Text = "+", FontSize = 22, Margin = new Thickness(10, 8, 10, 0) });
                var cap = new Border { CornerRadius = new CornerRadius(6), BorderThickness = new Thickness(1), Padding = new Thickness(16, 10, 16, 10), Margin = new Thickness(0, 0, 0, 4),
                    Child = new TextBlock { Text = keys[i], FontSize = 19, FontWeight = FontWeights.SemiBold } };
                cap.SetResourceReference(BackgroundProperty, "ControlFillColorDefaultBrush");
                cap.SetResourceReference(Border.BorderBrushProperty, "ControlStrokeColorDefaultBrush");
                Keycaps.Children.Add(cap);
                if (animate) OptionsMotion.Pop(cap, i * 40);
            }
        }

        private void ChangeClick(object sender, RoutedEventArgs e)
        {
            if (IsCapturing) { CancelCapture(); return; }
            IsCapturing = true;
            int version = ++_captureVersion;
            Error.Visibility = Visibility.Collapsed;
            Change.Content = "Cancel";
            ShortcutCard.SetResourceReference(Border.BorderBrushProperty, "AccentFillColorDefaultBrush");
            Keycaps.Children.Clear();
            Keycaps.Children.Add(new TextBlock { Text = "Press your shortcut\u2026", FontSize = 20, Margin = new Thickness(0, 10, 0, 0) });
            _hook.BeginCapture(key => Dispatcher.BeginInvoke(new Action(() =>
            {
                if (version != _captureVersion) return;
                CancelCapture();
                if (key == null) return;
                if (!key.IsValid) { Error.Visibility = Visibility.Visible; return; }
                _settings.Hotkey = key; _settings.Save(); ShowKeys(true);
            })));
        }

        public void CancelCapture()
        {
            if (!IsCapturing) return;
            IsCapturing = false; ++_captureVersion;
            _hook.CancelCapture();
            Change.Content = "Change\u2026";
            ShortcutCard.SetResourceReference(Border.BorderBrushProperty, "CardStrokeColorDefaultBrush");
            ShowKeys(false);
        }

        private void ResetClick(object sender, RoutedEventArgs e)
        {
            CancelCapture(); Error.Visibility = Visibility.Collapsed;
            _settings.Hotkey = Hotkey.Default; _settings.Save(); ShowKeys(true);
        }
    }
}
