using System;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Threading;
using Microsoft.Win32;

namespace WispX
{
    /// <summary>
    /// WispX installs itself: run the downloaded WispX.exe and it copies itself to
    /// %LOCALAPPDATA%\Programs\WispX, adds a Start menu entry, and launches from there.
    /// Running a newer download later replaces the installed copy (that's the update path).
    /// </summary>
    internal static class Installer
    {
        public const string QuitEventName = @"Local\WispX.Quit.v1";

        public static readonly string Dir =
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Programs", "WispX");
        public static string InstalledExe => Path.Combine(Dir, "WispX.exe");
        private static string ShortcutPath =>
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Programs), "WispX.lnk");

        /// <returns>true if this process handed off to the installed copy and should exit.</returns>
        public static bool EnsureInstalled(string[] args)
        {
            string self = Environment.ProcessPath;
            if (self == null) return false;
            if (string.Equals(Path.GetFullPath(self), Path.GetFullPath(InstalledExe), StringComparison.OrdinalIgnoreCase))
            {
                TryCreateShortcut();
                return false;
            }
            if (args.Contains("--portable") || Debugger.IsAttached || self.Contains(@"\bin\")) return false;

            // Ask a running copy to quit so its file can be replaced.
            try
            {
                using var quit = EventWaitHandle.OpenExisting(QuitEventName);
                quit.Set();
            }
            catch { }
            foreach (var p in Process.GetProcessesByName("WispX").Where(p => p.Id != Environment.ProcessId))
            {
                try { if (!p.WaitForExit(4000)) p.Kill(); } catch { }
            }

            try
            {
                Directory.CreateDirectory(Dir);
                File.Copy(self, InstalledExe, overwrite: true);
            }
            catch
            {
                return false; // couldn't install; just run from where we are
            }
            TryCreateShortcut();
            Process.Start(new ProcessStartInfo(InstalledExe) { UseShellExecute = true, WorkingDirectory = Dir });
            return true;
        }

        private static void TryCreateShortcut()
        {
            try
            {
                var t = Type.GetTypeFromProgID("WScript.Shell");
                dynamic shell = Activator.CreateInstance(t);
                dynamic lnk = shell.CreateShortcut(ShortcutPath);
                lnk.TargetPath = InstalledExe;
                lnk.WorkingDirectory = Dir;
                lnk.IconLocation = InstalledExe + ",0";
                lnk.Description = "WispX: hold Ctrl+Win to dictate with ChatGPT";
                lnk.Save();
            }
            catch { }
        }

        /// <summary>Removes the shortcut, startup entry, settings and the install folder.</summary>
        public static void Uninstall()
        {
            try { File.Delete(ShortcutPath); } catch { }
            try { using var k = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", true); k?.DeleteValue("WispX", false); } catch { }
            try { Directory.Delete(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "WispX"), true); } catch { }
            // The exe can't delete itself while running, so a short-lived cmd does it after we exit.
            try
            {
                Process.Start(new ProcessStartInfo("cmd.exe", $"/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{Dir}\"")
                { CreateNoWindow = true, UseShellExecute = false });
            }
            catch { }
        }
    }
}
