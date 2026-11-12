#!/bin/bash
# Run an end-to-end scenario against canoe, on River or standalone.
#
#   tools/e2e/run.sh <river|smithay> <scenario-file> <out-dir> [canoe-binary]
#
# The compositor runs headless at 1280x720 (CANOE_E2E_OUTPUTS to change,
# e.g. "1280x720,1280x720@1.5"), with an empty HOME (so no user config,
# or tools/e2e/<scenario>.toml if present) and its own XDG_RUNTIME_DIR.
#
# Scenario lines:
#   # comment
#   shot NAME            screenshot to <out-dir>/NAME.png (with cursor)
#   toplevels NAME       taskbar view (wltoplevel list) to <out-dir>/NAME.txt
#   spawn CMD...         run CMD in the background inside the session
#   run CMD...           run CMD in the foreground inside the session
#   sleep MS
#   anything else        wlinput commands; separate several with ';' to run
#                        them in one process (e.g. to hold a key)
set -u
MODE=$1 SCENARIO=$2 OUT=$3
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
WLINPUT=${WLINPUT:-$ROOT/tools/wlinput/target/release/wlinput}
OUTPUTS=${CANOE_E2E_OUTPUTS:-1280x720}
hdr=$(grep -m1 '^#! outputs ' "$SCENARIO" | cut -d' ' -f3)
[ -n "$hdr" ] && OUTPUTS=$hdr
WLTEST=${WLTEST:-$ROOT/tools/wlinput/target/release/wltest}
WLTOPLEVEL=${WLTOPLEVEL:-$ROOT/tools/wlinput/target/release/wltoplevel}
export WLTEST WLINPUT WLTOPLEVEL
BASE=/tmp/cne2e-$MODE
rm -rf "$BASE"; mkdir -p "$BASE/rt" "$BASE/home/.config/canoe" "$OUT"
chmod 700 "$BASE/rt"
cfg="${SCENARIO%.*}.toml"
[ -f "$cfg" ] && cp "$cfg" "$BASE/home/.config/canoe/canoe.toml"
export XDG_RUNTIME_DIR=$BASE/rt HOME=$BASE/home OUT
unset WAYLAND_DISPLAY DISPLAY
PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; pkill -P $$ 2>/dev/null; }
trap cleanup EXIT

if [ "$MODE" = river ]; then
    BIN=${4:-${CANOE_MAIN_BIN:?set CANOE_MAIN_BIN to a canoe built from main}}
    export CANOE_BIN=$BIN
    n=$(echo "$OUTPUTS" | tr ',' '\n' | wc -l)
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=$n \
        river -no-xwayland -c "echo \$WAYLAND_DISPLAY > $BASE/display; exec $BIN" >"$BASE/log" 2>&1 &
    PIDS+=($!)
    for i in $(seq 50); do [ -s "$BASE/display" ] && break; sleep 0.1; done
    export WAYLAND_DISPLAY=$BASE/rt/$(cat "$BASE/display")
    sleep 0.5
    # Give River's headless outputs the requested modes and layout.
    x=0; i=1
    for spec in $(echo "$OUTPUTS" | tr ',' ' '); do
        size=${spec%@*}; scale=1; [[ $spec == *@* ]] && scale=${spec#*@}
        wlr-randr --output HEADLESS-$i --custom-mode "$size" --scale "$scale" --pos "$x,0" >/dev/null 2>&1
        w=${size%x*}; x=$(python3 -c "print($x+round($w/$scale))"); i=$((i+1))
    done
else
    BIN=${4:-$ROOT/target/release/canoe}
    export CANOE_BIN=$BIN
    CANOE_TRACE_WM=${CANOE_TRACE_WM:-} CANOE_HEADLESS_OUTPUTS=$OUTPUTS "$BIN" --backend headless >"$BASE/log" 2>&1 &
    PIDS+=($!)
    for i in $(seq 50); do s=$(grep -o 'WAYLAND_DISPLAY=[^ ]*' "$BASE/log" | cut -d= -f2); [ -n "$s" ] && break; sleep 0.1; done
    export WAYLAND_DISPLAY=$BASE/rt/$s
fi
sleep 0.5

while IFS= read -r line || [ -n "$line" ]; do
    line="${line#"${line%%[![:space:]]*}"}"
    [ -z "$line" ] && continue
    case "$line" in
        \#*) ;;
        shot\ *) sleep 0.35; grim -c "$OUT/${line#shot }.png" ;;
        toplevels\ *) sleep 0.2; "$WLTOPLEVEL" list >"$OUT/${line#toplevels }.txt" ;;
        spawn\ *) bash -c "${line#spawn }" >>"$BASE/clients.log" 2>&1 & PIDS+=($!) ;;
        run\ *) bash -c "${line#run }" ;;
        sleep\ *) sleep "$(python3 -c "print(${line#sleep }/1000)")" ;;
        *)
            IFS=';' read -ra parts <<<"$line"
            args=(); for p in "${parts[@]}"; do p="${p#"${p%%[![:space:]]*}"}"; args+=("${p%"${p##*[![:space:]]}"}"); done
            # Create the virtual devices first: on River the seat only has
            # input capabilities while they exist, and clients need a moment
            # to bind them, else the first event gets lost.
            if [ ${#args[@]} -eq 1 ] && [[ ${args[0]} == wait-* || ${args[0]} == toplevels ]]; then
                "$WLINPUT" ${args[0]}
            else
                "$WLINPUT" "devices 150" "${args[@]}"
            fi || echo "wlinput failed: $line" >&2
            ;;
    esac
done <"$SCENARIO"
cp "$BASE/log" "$OUT/compositor.log"
[ -f "$BASE/clients.log" ] && cp "$BASE/clients.log" "$OUT/clients.log"
