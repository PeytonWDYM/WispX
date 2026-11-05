using System;
using System.Collections.Generic;
using System.Linq;
using System.Text.Json;
using System.Text.Json.Serialization;
using static WispX.Native;

namespace WispX
{
    /// <summary>Immutable modifier chord; multi-key chords match either side of each modifier.</summary>
    [JsonConverter(typeof(HotkeyConverter))]
    internal sealed class Hotkey
    {
        private readonly int[] _keys;
        private static readonly int[] Modifiers = { VK_LCONTROL, VK_RCONTROL, VK_LWIN, VK_RWIN, VK_LMENU, VK_RMENU, VK_LSHIFT, VK_RSHIFT };
        public static Hotkey Default { get; } = new(VK_LCONTROL, VK_LWIN);
        public IReadOnlyList<int> Keys => Array.AsReadOnly(_keys);
        public bool IsValid => _keys.Length >= 2 || _keys.Length == 1 && _keys[0] is VK_RCONTROL or VK_RMENU;
        public string Display => string.Join(" + ", _keys.Select(k => Name(k, _keys.Length == 1)));
        public bool MasksMenu => _keys.Any(k => Group(k) is 1 or 2);

        public Hotkey(params int[] keys)
        {
            if (keys.Length == 0 || keys.Any(k => !Modifiers.Contains(k))) throw new ArgumentException("Use modifier keys.", nameof(keys));
            _keys = keys.Distinct().OrderBy(Group).ThenBy(k => k).ToArray();
            if (_keys.Length > 1) _keys = _keys.GroupBy(Group).Select(g => Modifiers[g.Key * 2]).ToArray();
        }

        public bool Matches(IReadOnlySet<int> held) => _keys.Length == 1 ? held.Contains(_keys[0]) : _keys.All(k => held.Any(h => Group(h) == Group(k)));
        public bool HasExtraModifiers(IReadOnlySet<int> held) => held.Any(h => _keys.Length == 1 ? h != _keys[0] : !_keys.Any(k => Group(h) == Group(k)));
        internal static bool IsModifier(int key) => Modifiers.Contains(key);
        private static int Group(int key) => Array.IndexOf(Modifiers, key) / 2;
        private static string Name(int key, bool sided) => (sided && key is VK_RCONTROL or VK_RWIN or VK_RMENU or VK_RSHIFT ? "Right " : "") +
            (Group(key) switch { 0 => "Ctrl", 1 => "Win", 2 => "Alt", _ => "Shift" });

        /// <summary>Readable JSON such as "Ctrl+Win" or "RightCtrl".</summary>
        private sealed class HotkeyConverter : JsonConverter<Hotkey>
        {
            public HotkeyConverter() { }
            public override Hotkey Read(ref Utf8JsonReader reader, Type type, JsonSerializerOptions options)
            {
                var keys = reader.GetString().Split('+').Select(s => s.Replace(" ", "") switch
                {
                    "Ctrl" => VK_LCONTROL, "RightCtrl" => VK_RCONTROL,
                    "Win" => VK_LWIN, "RightWin" => VK_RWIN,
                    "Alt" => VK_LMENU, "RightAlt" => VK_RMENU,
                    "Shift" => VK_LSHIFT, "RightShift" => VK_RSHIFT,
                    _ => throw new JsonException("Unknown modifier.")
                }).ToArray();
                var hotkey = new Hotkey(keys);
                if (!hotkey.IsValid) throw new JsonException("Unsupported shortcut.");
                return hotkey;
            }
            public override void Write(Utf8JsonWriter writer, Hotkey value, JsonSerializerOptions options) => writer.WriteStringValue(value.Display.Replace(" ", ""));
        }
    }
}
