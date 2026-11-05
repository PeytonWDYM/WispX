using System;
using System.Collections.Generic;
using System.Linq;
using System.Text.Json;
using System.Text.Json.Serialization;
using Forms = System.Windows.Forms;
using static WispX.Native;

namespace WispX
{
    /// <summary>
    /// Immutable modifiers and an optional main key. Modifiers match either side, except a lone
    /// modifier with no main key (Right Ctrl / Right Alt), which is side-specific.
    /// </summary>
    [JsonConverter(typeof(HotkeyConverter))]
    internal sealed class Hotkey
    {
        private readonly int[] _keys;
        private readonly int[] _modifiers;
        private static readonly int[] Modifiers = { VK_LCONTROL, VK_RCONTROL, VK_LWIN, VK_RWIN, VK_LMENU, VK_RMENU, VK_LSHIFT, VK_RSHIFT };
        public static Hotkey Default { get; } = new(VK_LCONTROL, VK_LWIN);
        public IReadOnlyList<int> Keys => Array.AsReadOnly(_keys);
        public int? MainKey { get; }
        public bool IsValid => MainKey is int key ? key is not (VK_ESCAPE or VK_SPACE) && (_modifiers.Length > 0 || key is >= VK_F1 and <= VK_F24) :
            _modifiers.Length >= 2 || _modifiers.Length == 1 && _modifiers[0] is VK_RCONTROL or VK_RMENU;
        public string Display => string.Join(" + ", _keys.Select(k => IsModifier(k) ? Name(k, Sided) : KeyName(k)));
        private bool Sided => MainKey == null && _modifiers.Length == 1;
        public bool MasksMenu => _modifiers.Any(k => Group(k) is 1 or 2);

        public Hotkey(params int[] keys)
        {
            int[] main = keys.Where(k => !IsModifier(k)).Distinct().ToArray();
            if (keys.Length == 0 || main.Length > 1 || keys.Any(k => k is < 1 or > 255)) throw new ArgumentException("Use modifiers and at most one main key.", nameof(keys));
            _modifiers = keys.Where(IsModifier).Distinct().OrderBy(Group).ThenBy(k => k).ToArray();
            MainKey = main.Length == 1 ? main[0] : null;
            if (!Sided) _modifiers = _modifiers.GroupBy(Group).Select(g => Modifiers[g.Key * 2]).ToArray();
            _keys = _modifiers.Concat(main).ToArray();
        }

        public bool Matches(IReadOnlySet<int> held) => Sided ? held.Contains(_modifiers[0]) : _modifiers.All(k => held.Any(h => Group(h) == Group(k)));
        public bool HasExtraModifiers(IReadOnlySet<int> held) => held.Any(h => Sided ? h != _modifiers[0] : !_modifiers.Any(k => Group(h) == Group(k)));
        internal static bool IsModifier(int key) => Modifiers.Contains(key);
        private static int Group(int key) => Array.IndexOf(Modifiers, key) / 2;
        private static string Name(int key, bool sided) => (sided && key is VK_RCONTROL or VK_RWIN or VK_RMENU or VK_RSHIFT ? "Right " : "") +
            (Group(key) switch { 0 => "Ctrl", 1 => "Win", 2 => "Alt", _ => "Shift" });

        /// <summary>Keep top-row digits readable; other VK names also parse through the Forms enum.</summary>
        private static string KeyName(int key) => key is >= 0x30 and <= 0x39 ? ((char)key).ToString() : ((Forms.Keys)key).ToString();

        /// <summary>Readable JSON such as "Ctrl+Shift+D", "F9", or "RightCtrl".</summary>
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
                    var name when name.Length == 1 && name[0] is >= '0' and <= '9' => name[0],
                    var name when Enum.TryParse<Forms.Keys>(name, out var key) && (int)key is >= 1 and <= 255 => (int)key,
                    _ => throw new JsonException("Unknown key.")
                }).ToArray();
                var hotkey = new Hotkey(keys);
                if (!hotkey.IsValid) throw new JsonException("Unsupported shortcut.");
                return hotkey;
            }
            public override void Write(Utf8JsonWriter writer, Hotkey value, JsonSerializerOptions options) => writer.WriteStringValue(value.Display.Replace(" ", ""));
        }
    }
}
