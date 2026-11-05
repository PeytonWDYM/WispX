using System.Reflection;
using System.Windows.Controls;

namespace WispX
{
    /// <summary>Immediate general preferences, with Windows startup stored in its existing Run entry.</summary>
    public partial class GeneralPage : UserControl
    {
        internal GeneralPage(Settings settings)
        {
            InitializeComponent();
            SoundsToggle.IsChecked = settings.Sounds;
            PillToggle.IsChecked = settings.IdlePill;
            StartupToggle.IsChecked = Settings.StartWithWindows;
            foreach (var toggle in new[] { SoundsToggle, PillToggle, StartupToggle })
            {
                toggle.Content = toggle.IsChecked == true ? "On" : "Off";
                toggle.Click += (s, e) =>
                {
                    bool on = toggle.IsChecked == true;
                    toggle.Content = on ? "On" : "Off";
                    if (toggle == SoundsToggle) settings.Sounds = on;
                    else if (toggle == PillToggle) settings.IdlePill = on;
                    else Settings.StartWithWindows = on;
                    settings.Save();
                };
            }
            Version.Text = $"WispX {Assembly.GetExecutingAssembly().GetName().Version.ToString(3)}";
        }
    }
}
