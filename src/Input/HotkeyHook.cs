using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading;
using static WispX.Native;

namespace WispX
{
    internal enum HotkeyEvent { Start, Lock, Release, StopLocked, CancelSilent, Cancel }

    /// <summary>Global hold/lock dictation and modifier capture, on a dedicated keyboard-hook thread.</summary>
    internal sealed class HotkeyHook : IDisposable
    {
        private readonly Action<HotkeyEvent> _raise;
        private Thread _thread;
        private uint _threadId;
        private IntPtr _hook;
        private LowLevelKeyboardProc _proc; // root the native callback
        private readonly HashSet<int> _held = new();
        private readonly object _captureGate = new();
        private Action<Hotkey> _capture;
        private int[] _largest = Array.Empty<int>();
        private bool _comboHeld, _swallowSpaceUp, _swallowEscUp;
        private volatile bool _active, _locked;
        private volatile Hotkey _hotkey = WispX.Hotkey.Default;
        public Hotkey Hotkey { get => _hotkey; set => _hotkey = value; }

        public HotkeyHook(Action<HotkeyEvent> raise) => _raise = raise;

        public void Start()
        {
            _thread = new Thread(Loop) { IsBackground = true, Name = "WispX.KeyboardHook" };
            _thread.SetApartmentState(ApartmentState.STA);
            _thread.Start();
        }

        /// <summary>The caller marshals completion to its UI thread. Null means Escape cancelled capture.</summary>
        public void BeginCapture(Action<Hotkey> done)
        {
            lock (_captureGate)
            {
                if (_active || _locked) Fire(HotkeyEvent.CancelSilent);
                ForceIdle();
                _comboHeld = false;
                _largest = Array.Empty<int>();
                _capture = done;
            }
        }

        public void CancelCapture() { lock (_captureGate) _capture = null; }
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
            // Low-level hooks normally report sided keys; normalize the generic variants too.
            vk = vk switch { VK_CONTROL => VK_LCONTROL, VK_MENU => VK_LMENU, VK_SHIFT => VK_LSHIFT, _ => vk };
            bool modifier = WispX.Hotkey.IsModifier(vk);
            lock (_captureGate)
            {
                if (modifier) { if (down) _held.Add(vk); else if (up) _held.Remove(vk); }
                if (_capture != null)
                {
                    if (modifier && down)
                    {
                        MaskStartMenu();
                        if (_held.Count > _largest.Length) _largest = _held.ToArray();
                    }
                    if (down && vk == VK_ESCAPE || modifier && up && _held.Count == 0 && _largest.Length > 0)
                    {
                        var done = _capture;
                        _capture = null;
                        _swallowEscUp = vk == VK_ESCAPE;
                        done(vk == VK_ESCAPE ? null : new Hotkey(_largest));
                    }
                    return new IntPtr(1);
                }
            }

            bool swallow = false;
            var hotkey = _hotkey;
            bool matches = hotkey.Matches(_held);
            if (modifier)
            {
                if (down && matches && !_comboHeld)
                {
                    _comboHeld = true;
                    if (hotkey.MasksMenu) MaskStartMenu();
                    if (_locked) { _locked = false; _active = false; Fire(HotkeyEvent.StopLocked); }
                    else if (!_active && !hotkey.HasExtraModifiers(_held)) { _active = true; Fire(HotkeyEvent.Start); }
                }
                else if (up && _comboHeld && !matches)
                {
                    _comboHeld = false;
                    if (_active && !_locked) { _active = false; Fire(HotkeyEvent.Release); }
                }
            }
            else if (vk == VK_ESCAPE)
            {
                if (down && (_active || _locked)) { ForceIdle(); _swallowEscUp = true; swallow = true; Fire(HotkeyEvent.Cancel); }
                else if (up && _swallowEscUp) { _swallowEscUp = false; swallow = true; }
            }
            else if (vk == VK_SPACE && (down ? (_comboHeld && _active && !_locked) : _swallowSpaceUp))
            {
                if (down) { _locked = true; _swallowSpaceUp = true; Fire(HotkeyEvent.Lock); }
                else _swallowSpaceUp = false;
                swallow = true;
            }
            else if (down && _comboHeld && _active && !_locked)
            {
                _active = false;
                Fire(HotkeyEvent.CancelSilent);
            }
            return swallow ? new IntPtr(1) : CallNextHookEx(_hook, nCode, wParam, lParam);
        }

        private static void MaskStartMenu() => SendKeys((VK_MASK, false), (VK_MASK, true));
        private void Fire(HotkeyEvent e) { try { _raise(e); } catch { } }
        public void Dispose() { if (_threadId != 0) PostThreadMessage(_threadId, WM_QUIT, IntPtr.Zero, IntPtr.Zero); }
    }
}
