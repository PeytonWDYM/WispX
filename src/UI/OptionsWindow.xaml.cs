using System;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;

namespace WispX
{
    /// <summary>A single Options window with independent pages and recording/capture cleanup on focus loss.</summary>
    public partial class OptionsWindow : Window
    {
        private readonly UserControl[] _pages;
        private readonly ShortcutPage _shortcut;
        private readonly AudioPage _audio;

        internal OptionsWindow(Settings settings, HotkeyHook hook, Func<bool> dictating, bool tour = false)
        {
            InitializeComponent();
            _shortcut = new ShortcutPage(settings, hook);
            _audio = new AudioPage(settings, dictating);
            _pages = new UserControl[] { new AccountPage(), _shortcut, _audio, new GeneralPage(settings) };
            if (tour) foreach (var page in _pages) { page.IsHitTestVisible = false; page.Focusable = false; KeyboardNavigation.SetTabNavigation(page, KeyboardNavigationMode.None); }
            Navigation.SelectedIndex = 0;
            Loaded += (s, e) =>
            {
                Surface.BeginAnimation(OpacityProperty, OptionsMotion.To(0, 1, 200));
                OpenScale.BeginAnimation(ScaleTransform.ScaleXProperty, OptionsMotion.To(0.97, 1, 200));
                OpenScale.BeginAnimation(ScaleTransform.ScaleYProperty, OptionsMotion.To(0.97, 1, 200));
            };
            Deactivated += (s, e) => { StopMicrophoneTest(); _shortcut.CancelCapture(); };
            Closed += (s, e) => { StopMicrophoneTest(); _shortcut.CancelCapture(); };
        }

        internal void SelectPage(int index) => Navigation.SelectedIndex = index;
        internal void StopMicrophoneTest() => _audio.StopTest();

        private void NavigationChanged(object sender, SelectionChangedEventArgs e)
        {
            StopMicrophoneTest(); _shortcut.CancelCapture();
            int index = Navigation.SelectedIndex;
            PageHost.Content = _pages[index];
            IndicatorShift.BeginAnimation(TranslateTransform.YProperty, OptionsMotion.To(IndicatorShift.Y, index * 50, 250));
            PageHost.BeginAnimation(OpacityProperty, OptionsMotion.To(0, 1, 220));
            PageShift.BeginAnimation(TranslateTransform.YProperty, OptionsMotion.To(12, 0, 220));
        }

        private void WindowKeyDown(object sender, KeyEventArgs e)
        {
            if (e.Key == Key.Escape)
            {
                if (_shortcut.IsCapturing) _shortcut.CancelCapture(); else Close();
                e.Handled = true;
            }
            else if (e.Key == Key.Tab && Keyboard.Modifiers.HasFlag(ModifierKeys.Control))
            {
                int step = Keyboard.Modifiers.HasFlag(ModifierKeys.Shift) ? -1 : 1;
                SelectPage((Navigation.SelectedIndex + step + 4) % 4); e.Handled = true;
            }
            else if ((e.Key is Key.Up or Key.Down or Key.Left or Key.Right) && Navigation.IsKeyboardFocusWithin)
            {
                int step = e.Key is Key.Up or Key.Left ? -1 : 1;
                SelectPage((Navigation.SelectedIndex + step + 4) % 4); e.Handled = true;
            }
        }
    }
}
