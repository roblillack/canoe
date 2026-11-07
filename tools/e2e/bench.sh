#!/bin/bash
# Compare the compositor-side cost of canoe on River and standalone.
#
#   tools/e2e/bench.sh <river|smithay> [canoe-binary]
#
# Both run headless with Pixman at 1280x720 and an empty config. Measures
# CPU time and memory of the compositor processes (River and the canoe
# window manager, or canoe alone), not of the clients.
set -u
MODE=$1
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
W=$ROOT/tools/wlinput/target/release/wlinput
BASE=/tmp/cnbench-$MODE
rm -rf "$BASE"; mkdir -p "$BASE/rt" "$BASE/home"; chmod 700 "$BASE/rt"
export XDG_RUNTIME_DIR=$BASE/rt HOME=$BASE/home
unset WAYLAND_DISPLAY DISPLAY
TICK=$(getconf CLK_TCK)

t_start=$(date +%s.%N)
if [ "$MODE" = river ]; then
    BIN=${2:-/tmp/claude-1000/-home-rob-dev-canoe/113295b5-117b-4933-8519-67c588c3f18b/scratchpad/canoe-main/target/release/canoe}
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1 \
        river -no-xwayland -c "echo \$WAYLAND_DISPLAY > $BASE/display; exec $BIN" >"$BASE/log" 2>&1 &
    MAIN=$!
    for i in $(seq 100); do [ -s "$BASE/display" ] && break; sleep 0.02; done
    export WAYLAND_DISPLAY=$BASE/rt/$(cat "$BASE/display")
    wlr-randr --output HEADLESS-1 --custom-mode 1280x720 >/dev/null 2>&1
else
    BIN=${2:-$ROOT/target/release/canoe}
    CANOE_HEADLESS_OUTPUTS=1280x720 "$BIN" --backend headless >"$BASE/log" 2>&1 &
    MAIN=$!
    for i in $(seq 100); do s=$(grep -o 'WAYLAND_DISPLAY=[^ ]*' "$BASE/log" | cut -d= -f2); [ -n "$s" ] && break; sleep 0.02; done
    export WAYLAND_DISPLAY=$BASE/rt/$s
fi
# Ready once a screenshot can be taken.
until grim - >/dev/null 2>&1; do sleep 0.01; done
t_ready=$(date +%s.%N)

pids() { echo $MAIN $(pgrep -P $MAIN -x canoe); }
cpu() { local t=0; for p in $(pids); do read -ra f < /proc/$p/stat; t=$((t + f[13] + f[14])); done; echo $t; }
rss() { local t=0; for p in $(pids); do t=$((t + $(awk '/VmRSS/{print $2}' /proc/$p/status))); done; echo $t; }
ms() { python3 -c "print(round(($2-$1)*1000/$TICK))"; }

echo "mode=$MODE startup_ms=$(python3 -c "print(round(($t_ready-$t_start)*1000))") rss_kb_empty=$(rss)"

# Five terminals; time from spawn to the window being listed.
open_ms=()
for i in 1 2 3 4 5; do
    t0=$(date +%s.%N)
    foot -a bench$i >/dev/null 2>&1 &
    $W "wait-toplevel bench$i 5000" >/dev/null
    t1=$(date +%s.%N)
    open_ms+=($(python3 -c "print(round(($t1-$t0)*1000))"))
    $W "devices 50" "drag 300 27 $((330 + i * 40)) $((60 + i * 40)) 10" >/dev/null
done
sleep 1
echo "window_open_ms=${open_ms[*]} rss_kb_5_windows=$(rss)"

c0=$(cpu); sleep 5; c1=$(cpu)
echo "idle_cpu_ms_per_5s=$(ms $c0 $c1)"

# Interactive move: 600 motion events dragging a titlebar around.
c0=$(cpu); t0=$(date +%s.%N)
$W "devices 50" "drag 520 267 1000 600 600" >/dev/null
t1=$(date +%s.%N); sleep 0.5; c1=$(cpu)
echo "move_600_steps: cpu_ms=$(ms $c0 $c1) wall_ms=$(python3 -c "print(round(($t1-$t0)*1000))")"

# Interactive resize from the bottom right corner (the client redraws).
c0=$(cpu); t0=$(date +%s.%N)
$W "devices 50" "keydown super" "drag 1100 700 700 500 300 right" "keyup super" >/dev/null
t1=$(date +%s.%N); sleep 0.5; c1=$(cpu)
echo "resize_300_steps: cpu_ms=$(ms $c0 $c1) wall_ms=$(python3 -c "print(round(($t1-$t0)*1000))")"

# Window switcher: 40 steps through the menu.
cmds=("devices 50" "keydown super"); for i in $(seq 40); do cmds+=("key Tab"); done; cmds+=("keyup super")
c0=$(cpu); $W "${cmds[@]}" >/dev/null; sleep 0.5; c1=$(cpu)
echo "alt_tab_40_steps: cpu_ms=$(ms $c0 $c1)"

# Pointer motion over windows (hover, no drag): 600 events.
c0=$(cpu); { echo "devices 50"; for x in $(seq 100 2 1298); do echo "move $x 400"; done; } | $W - >/dev/null
sleep 0.5; c1=$(cpu)
echo "pointer_motion_600: cpu_ms=$(ms $c0 $c1)"
echo "rss_kb_end=$(rss)"
kill $MAIN; wait 2>/dev/null
