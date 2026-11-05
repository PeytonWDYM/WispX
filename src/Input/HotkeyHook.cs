using System;
using System.Runtime.InteropServices;
using System.Threading;
using static WispX.Native;

namespace WispX
{
    internal enum HotkeyEvent { Start, Lock, Release, StopLocked, CancelSilent, Cancel }

    /// <summary>
    /// Global Ctrl+Win push-to-talk detector built on a low-level keyboard hook.
    /// Runs on its own thread so a busy UI can never make Windows drop the hook.
    ///
    ///  hold Ctrl+Win            -> record, release to transcribe
    ///  Ctrl+Win, then Space     -> lock (hands-free); press Ctrl+Win again to finish
    ///  Esc                      -> cancel
    ///  Ctrl+Win + any other key -> you meant a Windows shortcut (e.g. Ctrl+Win+D), so we quietly back off
    /// </summary>
    internal sealed class HotkeyHook : IDisposable
    {
        private readonly Action<HotkeyEvent> _raise;
        private Thread _thread;
        private uint _threadId;
        private IntPtr _hook;
        private LowLevelKeyboardProc _proc; // keep a reference so the GC doesn't eat it

        private bool _lctrl, _rctrl, _lwin, _rwin;
        private bool _comboHeld, _swallowSpaceUp, _swallowEscUp;
        private volatile bool _active, _locked;

        public HotkeyHook(Action<HotkeyEvent> raise) => _raise = raise;

        public void Start()
        {
            _thread = new Thread(Loop) { IsBackground = true, Name = "WispX.KeyboardHook" };
            _thread.SetApartmentState(ApartmentState.STA);
            _thread.Start();
        }

        /// <summary>Called by the UI when it ends a session on its own (errors, max length, etc).</summary>
        public void ForceIdle() { _active = false; _locked = false; }

        private void Loop()
        {
            _threadId = GetCurrentThreadId();
            _proc = Callback;
            _hook = SetWindowsHookEx(WH_KEYBOARD_LL, _proc, GetModuleHandle(null), 0);
            while (GetMessage(out MSG msg, IntPtr.Zero, 0, 0) > 0) { }
            if (_hook != IntPtr.Zero) UnhookWindowsHookEx(_hook);
        }

        private IntPtr Callback(int nCode, IntPtr wParam, IntPtr lParam)
        {
            if (nCode < 0) return CallNextHookEx(_hook, nCode, wParam, lParam);

            var k = Marshal.PtrToStructure<KBDLLHOOKSTRUCT>(lParam);
            if ((k.flags & LLKHF_INJECTED) != 0) return CallNextHookEx(_hook, nCode, wParam, lParam);

            int msg = wParam.ToInt32();
            bool down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            bool up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            int vk = (int)k.vkCode;
            bool swallow = false;

            bool isCtrlWin = true;
            switch (vk)
            {
                case VK_LCONTROL: _lctrl = down; break;
                case VK_RCONTROL: _rctrl = down; break;
                case VK_CONTROL: _lctrl = down; break;
                case VK_LWIN: _lwin = down; break;
                case VK_RWIN: _rwin = down; break;
                default: isCtrlWin = false; break;
            }
            bool both = (_lctrl || _rctrl) && (_lwin || _rwin);

            if (isCtrlWin)
            {
                if (down && both && !_comboHeld)
                {
                    _comboHeld = true;
                    MaskStartMenu();
                    if (_locked) { _locked = false; _active = false; Fire(HotkeyEvent.StopLocked); }
                    else if (!_active && !IsDown(VK_SHIFT) && !IsDown(VK_MENU)) { _active = true; Fire(HotkeyEvent.Start); }
                }
                else if (up && _comboHeld && !both)
                {
                    _comboHeld = false;
                    if (_active && !_locked) { _active = false; Fire(HotkeyEvent.Release); }
                }
            }
            else if (vk == VK_ESCAPE)
            {
                if (down && (_active || _locked)) { _active = false; _locked = false; _swallowEscUp = true; swallow = true; Fire(HotkeyEvent.Cancel); }
                else if (up && _swallowEscUp) { _swallowEscUp = false; swallow = true; }
            }
            else if (vk == VK_SPACE && (down ? (_comboHeld && _active && !_locked) : _swallowSpaceUp))
            {
                if (down) { _locked = true; _swallowSpaceUp = true; Fire(HotkeyEvent.Lock); }
                else _swallowSpaceUp = false;
                swallow = true;
            }
            else if (down && _comboHeld && _active && !_locked && !IsModifier(vk))
            {
                // Ctrl+Win+<key> is a Windows shortcut (virtual desktops, etc). Get out of the way.
                _active = false;
                Fire(HotkeyEvent.CancelSilent);
            }

            return swallow ? new IntPtr(1) : CallNextHookEx(_hook, nCode, wParam, lParam);
        }

        private static bool IsModifier(int vk) =>
            vk is VK_SHIFT or VK_LSHIFT or VK_RSHIFT or VK_MENU or VK_LMENU or VK_RMENU;

        private static void MaskStartMenu() => SendKeys((VK_MASK, false), (VK_MASK, true));

        private void Fire(HotkeyEvent e) { try { _raise(e); } catch { } }

        public void Dispose()
        {
            if (_threadId != 0) PostThreadMessage(_threadId, WM_QUIT, IntPtr.Zero, IntPtr.Zero);
        }
    }
}
