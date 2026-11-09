#!/usr/bin/env bash
# Starts WispX in a private, headless Plasma session and checks the desktop
# integration that doesn't need a person: Wayland and X11 (XWayland) startup,
# the layer-shell pill, portal shortcut binding, single instance, quit, and
# Options screenshots. Nothing touches the real session: the compositor, D-Bus
# bus, HOME and XDG directories are all private and deleted afterwards.
#
# It doesn't press keys or speak into a microphone, and the scratch HOME has no
# Codex login. tests/linux_nested.rs presses the shortcuts and checks pasting.
#
#   bash scripts/linux-smoke.sh [path/to/WispX]
set -euo pipefail
cd "$(dirname "$0")/.."
exe="$(realpath "${1:-target/release/WispX}")"
out="$PWD/artifacts/linux-smoke"
for tool in dbus-daemon kwin_wayland qdbus6 timeout; do
    command -v "$tool" >/dev/null || { echo "skipped: $tool is not installed"; exit 0; }
done
[[ -x "$exe" ]] || { echo "build first: cargo build --release"; exit 1; }

root="$(mktemp -d /tmp/wispx-smoke.XXXXXX)"
rm -rf "$out"
mkdir -p "$out" "$root"/{home,config,data,cache,run,run-x11}
chmod 700 "$root/run" "$root/run-x11"
socket="wayland-wispx-smoke"
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    cp "$root"/*.log "$out"/ 2>/dev/null || true
    rm -rf "$root"
}
trap cleanup EXIT

base=(
    HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data"
    XDG_CACHE_HOME="$root/cache" XDG_RUNTIME_DIR="$root/run" PATH=/usr/bin:/bin
    LANG=C.UTF-8 XDG_CURRENT_DESKTOP=KDE KDE_FULL_SESSION=true KDE_SESSION_VERSION=6
    XDG_SESSION_TYPE=wayland QT_QPA_PLATFORM=wayland KWIN_COMPOSE=Q
    # Known before KWin starts, so portals the bus activates can reach it.
    WAYLAND_DISPLAY="$socket" QT_LOGGING_RULES="kwin_scripting=true" QT_FORCE_STDERR_LOGGING=1
    # Software compositing can't import GPU buffers from XWayland or Qt dialogs.
    XWAYLAND_NO_GLAMOR=1 QT_QUICK_BACKEND=software QSG_RHI_BACKEND=software
)
run() { env -i "${base[@]}" "$@"; }

timeout 300 env -i "${base[@]}" dbus-daemon --session --nofork --print-address=1 \
    --address="unix:path=$root/bus" >"$root/bus-address" 2>"$root/bus.log" &
pids+=($!)
for _ in $(seq 100); do [[ -s "$root/bus-address" ]] && break; sleep 0.05; done
base+=(DBUS_SESSION_BUS_ADDRESS="$(head -1 "$root/bus-address")")

# The X11 half runs as KWin's child, which is how it learns XWayland's DISPLAY.
# Its own runtime directory keeps it from handing off to the Wayland instance.
cat >"$root/x11.sh" <<EOF
#!/bin/sh
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="$root/run-x11"
echo "\$DISPLAY" > "$root/x11-display"
"$exe" --diagnose > "$root/x11-diagnose.log" 2>&1
"$exe" > "$root/x11-app.log" 2>&1 &
echo \$! > "$root/x11-pid"
EOF
chmod +x "$root/x11.sh"
timeout 300 env -i "${base[@]}" kwin_wayland --virtual --xwayland --no-lockscreen \
    --width 1280 --height 800 --socket "$socket" "$root/x11.sh" >"$root/kwin.log" 2>&1 &
pids+=($!)
for _ in $(seq 300); do [[ -S "$root/run/$socket" ]] && break; sleep 0.1; done
[[ -S "$root/run/$socket" ]] || { echo "FAIL: headless KWin didn't start"; exit 1; }

failures=0
check() { if "${@:2}"; then echo "ok: $1"; else echo "FAIL: $1"; failures=$((failures + 1)); fi; }
running() { kill -0 "$1" 2>/dev/null; }
wait_gone() { for _ in $(seq 100); do running "$1" || return 0; sleep 0.1; done; return 1; }

run "$exe" --diagnose >"$root/wayland-diagnose.log" 2>&1
check "diagnose sees the layer shell" grep -q "pill: layer shell" "$root/wayland-diagnose.log"
check "diagnose sees the GlobalShortcuts portal" grep -q "GlobalShortcuts v" "$root/wayland-diagnose.log"
cat "$root/wayland-diagnose.log"

run "$exe" >"$root/wayland-app.log" 2>&1 &
app=$!
pids+=("$app")
for _ in $(seq 100); do [[ -S "$root/run/WispX/instance.sock" ]] && break; sleep 0.1; done
check "Wayland instance is listening" test -S "$root/run/WispX/instance.sock"
check "launcher entry written" test -f "$root/data/applications/io.github.PeytonWDYM.WispX.desktop"
sleep 3
second() { timeout 10 env -i "${base[@]}" "$exe" >"$root/second.log" 2>&1; }
check "second launch hands off and exits" second
check "first instance still runs" running "$app"

# List KWin's windows from a throwaway script; print() lands in kwin.log.
cat >"$root/windows.js" <<'EOF'
for (const w of workspace.stackingOrder) {
    print("WISPX-WINDOW " + w.resourceClass + " | " + w.caption + " | " + w.width + "x" + w.height
        + " | keepAbove=" + w.keepAbove + " | wantsInput=" + w.wantsInput);
}
EOF
windows() {
    local name="wispx-smoke-$RANDOM" id
    id="$(run qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript "$root/windows.js" "$name")" || return 0
    run qdbus6 org.kde.KWin "/Scripting/Script$id" org.kde.kwin.Script.run >/dev/null || true
    sleep 0.5
    run qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript "$name" >/dev/null || true
    { grep "WISPX-WINDOW" "$root/kwin.log" || true; } | sed 's/^.*WISPX-WINDOW/window:/' | sort -u >"$root/windows.log"
}
# A fresh font cache makes the first Options window slow to appear.
for _ in $(seq 30); do windows; grep -q "WispX Options" "$root/windows.log" && break; sleep 0.5; done
cat "$root/windows.log"
check "Options opened on the second launch" grep -q "WispX Options" "$root/windows.log"
check "the pill never takes input" bash -c "grep '| 480x120 |' '$root/windows.log' | grep -q 'wantsInput=false'"

run "$exe" --quit || true
check "--quit stops the Wayland instance" wait_gone "$app"

x11pid="$(cat "$root/x11-pid" 2>/dev/null || true)"
echo "XWayland display: $(cat "$root/x11-display" 2>/dev/null || echo none)"
cat "$root/x11-diagnose.log" 2>/dev/null || true
check "X11 diagnose ran" grep -q "session: X11" "$root/x11-diagnose.log"
if [[ -n "$x11pid" ]]; then
    check "X11 instance stays up" running "$x11pid"
    run env XDG_RUNTIME_DIR="$root/run-x11" "$exe" --quit || true
    check "--quit stops the X11 instance" wait_gone "$x11pid"
fi

timeout 60 env -i "${base[@]}" "$exe" --options-tour "$out/options" >"$root/tour.log" 2>&1 || true
check "Options tour saved five pages" test "$(ls "$out/options" 2>/dev/null | wc -l)" -eq 5
"$exe" --pill-tour "$out/pill" >/dev/null || true
check "pill tour painted every state" test "$(ls "$out/pill" 2>/dev/null | wc -l)" -eq 14

for log in "$root"/wayland-app.log "$root"/x11-app.log; do
    check "no panic in $(basename "$log")" bash -c "! grep -q panicked '$log'"
done
echo "logs and pictures: $out"
exit "$failures"
