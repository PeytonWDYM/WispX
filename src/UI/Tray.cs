using System;
using Forms = System.Windows.Forms;

namespace WispX
{
    public partial class App
    {
        /// <summary>Tray actions share the same live preferences as Options.</summary>
        private void BuildTray()
        {
            System.Drawing.Icon icon;
            try { icon = System.Drawing.Icon.ExtractAssociatedIcon(Environment.ProcessPath) ?? System.Drawing.SystemIcons.Application; }
            catch { icon = System.Drawing.SystemIcons.Application; }
            var menu = new Forms.ContextMenuStrip { ShowImageMargin = false, ShowCheckMargin = false };
            _title = new Forms.ToolStripMenuItem($"WispX \u00B7 hold {_settings.Hotkey.Display}") { Enabled = false };
            _status = new Forms.ToolStripMenuItem("") { Enabled = false };
            _copyLast = new Forms.ToolStripMenuItem("Copy last transcript", null, (s, e) =>
            {
                if (_lastTranscript != null) Injector.SetText(_lastTranscript, excludeFromHistory: false);
            }) { Enabled = false };
            var options = new Forms.ToolStripMenuItem("Options\u2026", null, (s, e) => OpenOptions())
            { Font = new System.Drawing.Font(menu.Font, System.Drawing.FontStyle.Bold) };
            var help = new Forms.ToolStripMenuItem("How to use", null, (s, e) =>
                _tray.ShowBalloonTip(7000, "WispX",
                    $"Hold {_settings.Hotkey.Display} \u2192 talk \u2192 let go. Text is typed where your cursor is.\n" +
                    $"{_settings.Hotkey.Display}, then Space \u2192 hands-free. {_settings.Hotkey.Display} again to finish.\nEsc \u2192 cancel.",
                    Forms.ToolTipIcon.None));
            var uninstall = new Forms.ToolStripMenuItem("Uninstall WispX\u2026", null, (s, e) =>
            {
                var ok = Forms.MessageBox.Show("Remove WispX from this PC?\n\nYour Codex/ChatGPT login isn't touched.",
                    "Uninstall WispX", Forms.MessageBoxButtons.OKCancel, Forms.MessageBoxIcon.None);
                if (ok != Forms.DialogResult.OK) return;
                Installer.Uninstall(); Quit();
            });
            var quit = new Forms.ToolStripMenuItem("Quit WispX", null, (s, e) => Quit());
            menu.Items.AddRange(new Forms.ToolStripItem[]
            {
                _title, _status, new Forms.ToolStripSeparator(), _copyLast, options,
                new Forms.ToolStripSeparator(), help, uninstall, quit
            });
            menu.Opening += (s, e) =>
            {
                options.Select();
                _status.Text = ChatGptTranscriber.HasLogin() ? "\u2713 Using your ChatGPT login from Codex" : "\u26A0 Not signed in. Open Codex and sign in";
            };
            _tray = new Forms.NotifyIcon { Icon = icon, Text = "WispX", ContextMenuStrip = menu, Visible = true };
            _tray.Text = $"WispX \u00B7 hold {_settings.Hotkey.Display}";
            _tray.MouseClick += (s, e) => { if (e.Button == Forms.MouseButtons.Left) OpenOptions(); };
        }
    }
}
