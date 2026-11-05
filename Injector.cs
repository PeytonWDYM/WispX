using System;
using System.Collections.Generic;
using System.IO;
using System.Threading.Tasks;
using System.Windows;
using static WispX.Native;

namespace WispX
{
    /// <summary>Types text into whatever app has focus by pasting it, then puts your clipboard back.</summary>
    internal static class Injector
    {
        /// <returns>true if pasted; false if focus moved and the text was only copied.</returns>
        public static async Task<bool> PasteAsync(string text, IntPtr expectedWindow)
        {
            // Never fire Ctrl+V while you're still physically holding Ctrl/Win/Shift/Alt.
            for (int i = 0; i < 150 && AnyModifierHeld(); i++) await Task.Delay(15);

            if (GetForegroundWindow() != expectedWindow)
            {
                // You switched windows while it was transcribing. Don't paste into the wrong place.
                SetText(text, excludeFromHistory: false);
                return false;
            }

            var backup = Backup();
            if (!SetText(text, excludeFromHistory: true)) return false;
            await Task.Delay(25);
            SendKeys((VK_CONTROL, false), (VK_V, false), (VK_V, true), (VK_CONTROL, true));
            await Task.Delay(500);

            try
            {
                if (backup != null && Clipboard.ContainsText() && Clipboard.GetText() == text)
                    Clipboard.SetDataObject(backup, true);
            }
            catch { }
            return true;
        }

        public static bool SetText(string text, bool excludeFromHistory)
        {
            var data = new DataObject();
            data.SetData(DataFormats.UnicodeText, text);
            data.SetData(DataFormats.Text, text);
            if (excludeFromHistory)
            {
                // Keep temporary dictation pastes out of Win+V history and cloud clipboard.
                data.SetData("ExcludeClipboardContentFromMonitorProcessing", new MemoryStream(new byte[] { 1, 0, 0, 0 }));
                data.SetData("CanIncludeInClipboardHistory", new MemoryStream(new byte[] { 0, 0, 0, 0 }));
                data.SetData("CanUploadToCloudClipboard", new MemoryStream(new byte[] { 0, 0, 0, 0 }));
            }
            for (int i = 0; i < 10; i++)
            {
                try { Clipboard.SetDataObject(data, true); return true; }
                catch { System.Threading.Thread.Sleep(30); }
            }
            return false;
        }

        private static DataObject Backup()
        {
            try
            {
                var src = Clipboard.GetDataObject();
                if (src == null) return null;
                var copy = new DataObject();
                int n = 0;
                foreach (var f in src.GetFormats(false))
                {
                    try { var v = src.GetData(f, false); if (v != null) { copy.SetData(f, v); n++; } }
                    catch { }
                }
                return n > 0 ? copy : null;
            }
            catch { return null; }
        }

        private static bool AnyModifierHeld() =>
            IsDown(VK_CONTROL) || IsDown(VK_LWIN) || IsDown(VK_RWIN) || IsDown(VK_SHIFT) || IsDown(VK_MENU);
    }
}
