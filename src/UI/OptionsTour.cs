using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Interop;
using System.Runtime.InteropServices;

namespace WispX
{
    /// <summary>Read-only visual verification in a separate process, without installation, tray, or keyboard hooks.</summary>
    internal static class OptionsTour
    {
        [DllImport("dwmapi.dll")] private static extern int DwmGetWindowAttribute(IntPtr hwnd, int attribute, out Native.RECT rect, int size);

        public static async Task RunAsync(OptionsWindow window, string outDir)
        {
            try
            {
                Directory.CreateDirectory(outDir);
                string[] names = { "1-account", "2-shortcut", "3-audio", "4-general" };
                for (int i = 0; i < names.Length; i++)
                {
                    window.SelectPage(i);
                    window.Activate();
                    await Task.Delay(600);
                    // Extended frame bounds are physical pixels, excluding the invisible resize border.
                    Marshal.ThrowExceptionForHR(DwmGetWindowAttribute(new WindowInteropHelper(window).Handle,
                        9 /* DWMWA_EXTENDED_FRAME_BOUNDS */, out var bounds, Marshal.SizeOf<Native.RECT>()));
                    using var bitmap = new Bitmap(bounds.Right - bounds.Left, bounds.Bottom - bounds.Top);
                    using (var graphics = Graphics.FromImage(bitmap))
                        graphics.CopyFromScreen(bounds.Left, bounds.Top, 0, 0, bitmap.Size);
                    bitmap.Save(Path.Combine(outDir, names[i] + ".png"), ImageFormat.Png);
                }
            }
            finally { window.Close(); Application.Current.Shutdown(); }
        }
    }
}
