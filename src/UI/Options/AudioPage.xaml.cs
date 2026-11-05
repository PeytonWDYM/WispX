using System;
using System.Linq;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Threading;

namespace WispX
{
    /// <summary>Device/language preferences and a level meter that owns a recorder only while testing.</summary>
    public partial class AudioPage : UserControl
    {
        private readonly Settings _settings;
        private readonly Func<bool> _dictating;
        private readonly Recorder _test = new();
        private readonly DispatcherTimer _timer = new() { Interval = TimeSpan.FromMilliseconds(33) };
        private readonly Border[] _segments = new Border[24];
        private sealed record LanguageChoice(string Name, string Code);

        internal AudioPage(Settings settings, Func<bool> dictating)
        {
            InitializeComponent();
            _settings = settings; _dictating = dictating;
            var devices = Recorder.InputDevices();
            Microphone.Items.Add("Windows default");
            foreach (string device in devices) Microphone.Items.Add(device);
            Microphone.SelectedIndex = devices.ToList().IndexOf(settings.Microphone) + 1;
            LanguagePicker.ItemsSource = new[]
            {
                new LanguageChoice("Auto-detect", ""), new("English", "en"), new("Spanish", "es"), new("French", "fr"),
                new("German", "de"), new("Italian", "it"), new("Portuguese", "pt"), new("Dutch", "nl"), new("Polish", "pl"),
                new("Russian", "ru"), new("Ukrainian", "uk"), new("Turkish", "tr"), new("Arabic", "ar"), new("Hindi", "hi"),
                new("Japanese", "ja"), new("Korean", "ko"), new("Chinese", "zh")
            };
            LanguagePicker.SelectedValue = settings.Language;
            for (int i = 0; i < _segments.Length; i++)
            {
                var segment = new Border { CornerRadius = new CornerRadius(2), Margin = new Thickness(1.5, 0, 1.5, 0) };
                _segments[i] = segment; Meter.Children.Add(segment);
            }
            SetLevel(0);
            Microphone.SelectionChanged += (s, e) =>
            {
                StopTest();
                _settings.Microphone = Microphone.SelectedIndex == 0 ? "" : (string)Microphone.SelectedItem;
                _settings.Save();
            };
            LanguagePicker.SelectionChanged += (s, e) => { _settings.Language = (string)LanguagePicker.SelectedValue; _settings.Save(); };
            _timer.Tick += (s, e) => { if (_dictating()) StopTest(); else SetLevel(_test.Level); };
            Unloaded += (s, e) => StopTest();
        }

        private void TestClick(object sender, RoutedEventArgs e)
        {
            if (Test.IsChecked != true) { StopTest(); return; }
            if (_dictating())
            {
                StopTest(); TestError.Text = "Finish dictating before testing your microphone."; TestError.Visibility = Visibility.Visible; return;
            }
            try
            {
                _test.Start(_settings.Microphone);
                TestError.Visibility = Visibility.Collapsed;
                _timer.Start();
            }
            catch
            {
                StopTest(); TestError.Text = "Couldn't open this microphone. Check its Windows settings."; TestError.Visibility = Visibility.Visible;
            }
        }

        public void StopTest()
        {
            _timer.Stop(); _test.Cancel(); Test.IsChecked = false; SetLevel(0);
        }

        private void SetLevel(float level)
        {
            int filled = (int)Math.Ceiling(level * _segments.Length);
            for (int i = 0; i < _segments.Length; i++)
                _segments[i].SetResourceReference(Border.BackgroundProperty, i < filled ? "AccentFillColorDefaultBrush" : "ControlAltFillColorTertiaryBrush");
        }
    }
}
