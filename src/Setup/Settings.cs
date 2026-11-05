using System;
using System.IO;
using System.Text.Json;
using Microsoft.Win32;

namespace WispX
{
    /// <summary>Per-user preferences, persisted atomically and applied live by the app.</summary>
    internal sealed class Settings
    {
        public bool Sounds { get; set; } = true;
        public bool IdlePill { get; set; } = true;
        /// <summary>Optional ISO language hint like "en". Blank = auto-detect.</summary>
        public string Language { get; set; } = "";
        public Hotkey Hotkey { get; set; } = Hotkey.Default;
        public string Microphone { get; set; } = "";
        public event Action Saved;
        public bool FirstRunDone { get; set; }

        private static readonly string Dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "WispX");
        private static readonly string FilePath = Path.Combine(Dir, "settings.json");

        public static Settings Load()
        {
            try { if (File.Exists(FilePath)) return JsonSerializer.Deserialize<Settings>(File.ReadAllText(FilePath)) ?? new Settings(); }
            catch { }
            return new Settings();
        }

        public void Save()
        {
            Directory.CreateDirectory(Dir);
            string tmp = FilePath + ".tmp";
            using (var stream = new FileStream(tmp, FileMode.Create, FileAccess.Write, FileShare.None))
            {
                JsonSerializer.Serialize(stream, this, new JsonSerializerOptions { WriteIndented = true });
                stream.Flush(true);
            }
            File.Move(tmp, FilePath, overwrite: true);
            Saved?.Invoke();
        }

        private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";

        public static bool StartWithWindows
        {
            get { using var k = Registry.CurrentUser.OpenSubKey(RunKey); return k?.GetValue("WispX") != null; }
            set
            {
                using var k = Registry.CurrentUser.CreateSubKey(RunKey);
                if (value) k.SetValue("WispX", $"\"{Environment.ProcessPath}\" --background");
                else k.DeleteValue("WispX", false);
            }
        }
    }
}
