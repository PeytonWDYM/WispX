using System;
using System.IO;
using System.Text.Json;
using Microsoft.Win32;

namespace WispX
{
    internal sealed class Settings
    {
        public bool Sounds { get; set; } = true;
        public bool IdlePill { get; set; } = true;
        /// <summary>Optional ISO language hint like "en". Blank = auto-detect.</summary>
        public string Language { get; set; } = "";
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
            try
            {
                Directory.CreateDirectory(Dir);
                File.WriteAllText(FilePath, JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true }));
            }
            catch { }
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
