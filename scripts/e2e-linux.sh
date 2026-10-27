#!/usr/bin/env bash
# End-to-end checks of the Linux build against a live X11 display.
#
#   scripts/e2e-linux.sh [path/to/airec] [evidence-dir]
#
# Needs DISPLAY plus xdotool, xmessage, xsetroot, ffmpeg, ffprobe, and jq. It drives the real
# pointer and changes the root window background, so run it on a disposable display where no
# desktop or panel window covers the root window. The window audio checks also need ffplay and
# a running PulseAudio or PipeWire server, and are skipped when doctor reports no audio.
set -euo pipefail

airec=$(realpath "${1:-target/release/airec}")
evidence=${2:-$(mktemp -d)}
mkdir -p "$evidence"
work=$(mktemp -d)
cd "$work"

fail() {
    echo "not ok - $*" >&2
    exit 1
}
pass() { echo "ok - $*"; }
probe() { ffprobe -v error -select_streams v:0 -show_entries "stream=$2" -of csv=p=0 "$1"; }
event() { jq -c "select(.event == \"$2\")" "$1" | tail -n 1; }
decodes_cleanly() {
    local errors
    errors=$(ffmpeg -v error -i "$1" -f null - 2>&1)
    [ -z "$errors" ] || { head -n 5 <<<"$errors" >&2; return 1; }
}
# Average RGB of a 16x16 block of the frame at the given second.
pixel() {
    ffmpeg -v error -ss "$2" -i "$1" -frames:v 1 -vf "crop=16:16:$3:$4,scale=1:1:flags=area" \
        -f rawvideo -pix_fmt rgb24 - | od -An -tu1 | xargs
}
near() {
    local -a actual=($1) expected=($2)
    for channel in 0 1 2; do
        local difference=$((actual[channel] - expected[channel]))
        [ "${difference#-}" -le "$3" ] || return 1
    done
}
# RMS level in dB of a narrow band of the audio track around the given frequency. One bandpass
# stage only attenuates a tone an octave away by about 17 dB, so two narrow stages are chained.
band_level() {
    local band="bandpass=f=$2:width_type=h:w=40"
    ffmpeg -hide_banner -nostats -i "$1" -map 0:a \
        -af "$band,$band,astats=measure_overall=RMS_level:measure_perchannel=none" \
        -f null - 2>&1 | awk '/RMS level dB/ { print $NF }'
}
louder_by() { awk -v a="$1" -v b="$2" -v m="$3" 'BEGIN { exit !(a - b >= m) }'; }
wait_for_window() {
    for _ in $(seq 50); do
        "$airec" list windows --json | jq -e --arg title "$1" 'any(.[]; .title == $title)' \
            >/dev/null && return 0
        sleep 0.1
    done
    return 1
}

doctor=$("$airec" doctor --json) || fail "doctor exited $?: $doctor"
jq -e '.x11_supported and .encoder.selected == "software" and .keyboard_hook == false' \
    <<<"$doctor" >/dev/null || fail "doctor reported $doctor"
pass "doctor finds X11 and the bundled encoder"

monitors=$("$airec" list monitors --json)
jq -e 'length >= 1 and ([.[] | select(.primary)] | length) == 1' <<<"$monitors" >/dev/null ||
    fail "monitors: $monitors"
width=$(jq '.[] | select(.primary) | .width' <<<"$monitors")
height=$(jq '.[] | select(.primary) | .height' <<<"$monitors")
pass "one primary monitor, ${width}x${height}"

# Monitor recording over a known background while the pointer clicks and drags.
xsetroot -solid '#7c3aed'
xdotool mousemove 200 200
(
    sleep 1.5
    xdotool mousemove 300 300 click 1
    sleep 0.5
    xdotool mousemove 500 300 mousedown 1 mousemove 560 340 mousemove 640 400 mouseup 1
    sleep 0.5
    xdotool mousemove 800 300 click 3
) &
"$airec" record --duration 5s --out monitor.mp4 --event-log events.jsonl --json >record.jsonl ||
    fail "monitor record exited $?"
wait
saved=$(event record.jsonl saved)
jq -e '.stop_reason == "duration_limit" and .frames >= 120' <<<"$saved" >/dev/null ||
    fail "monitor saved event: $saved"
[ "$(probe monitor.mp4 codec_name,width,height)" = "h264,$width,$height" ] ||
    fail "monitor stream: $(probe monitor.mp4 codec_name,width,height)"
# ffprobe prints these in its own order: range, matrix, transfer, primaries.
colour=$(probe monitor.mp4 color_range,color_space,color_transfer,color_primaries)
[ "$colour" = "tv,bt709,bt709,bt709" ] || fail "colour tags: $colour"
decodes_cleanly monitor.mp4 || fail "monitor.mp4 has decode errors"
duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 monitor.mp4)
awk -v d="$duration" 'BEGIN { exit !(d >= 4.5 && d <= 5.5) }' || fail "monitor duration $duration"
pass "5 s monitor recording is ${duration}s of tagged BT.709 H.264 that decodes cleanly"

background=$(pixel monitor.mp4 3 $((width - 40)) $((height - 40)))
near "$background" "124 58 237" 4 || fail "background #7c3aed decoded as $background"
pass "background #7c3aed decodes as $background"

jq -e -s '
    any(.[]; .event == "click" and .button == "left") and
    any(.[]; .event == "click" and .button == "right") and
    any(.[]; .event == "drag_start") and any(.[]; .event == "drag_end") and
    all(.[]; has("key") | not)
' events.jsonl >/dev/null || fail "event log: $(cat events.jsonl)"
pass "event log has left and right clicks and a drag, and no keyboard data"

pointer=$(pixel monitor.mp4 1 200 200)
near "$pointer" "124 58 237" 12 && fail "no pointer drawn at its resting place: $pointer"
pass "pointer is composited into the frame ($pointer)"

# The ripple grows around the left click at (300,300); sample left of it, where the pointer
# arrow, which extends right and down from its hotspot, never is.
click_ms=$(jq -s 'map(select(.event == "click" and .button == "left"))[0].t_ms' events.jsonl)
ripple_seen=false
for offset_ms in 50 150 250 350; do
    second=$(awk -v t="$click_ms" -v o="$offset_ms" 'BEGIN { printf "%.3f", (t + o) / 1000 }')
    near "$(pixel monitor.mp4 "$second" 276 292)" "124 58 237" 12 || ripple_seen=true
done
$ripple_seen || fail "no click effect next to the left click"
pass "left click effect is drawn into the frames"

# A recorded window that closes ends the recording with TARGET_LOST.
xmessage -title airec-e2e-window -geometry 480x200+60+60 "airec e2e" &
window_process=$!
wait_for_window airec-e2e-window || fail "test window never appeared"
(
    sleep 3
    kill "$window_process"
) &
set +e
"$airec" record --window airec-e2e-window --duration 30s --out window.mp4 --json >window.jsonl
code=$?
set -e
wait
[ "$code" = 8 ] || fail "window close exit code $code: $(cat window.jsonl)"
event window.jsonl target_lost | jq -e '.data.cause == "window_closed"' >/dev/null ||
    fail "target_lost event: $(cat window.jsonl)"
event window.jsonl saved | jq -e '.stop_reason == "target_lost"' >/dev/null ||
    fail "window saved event: $(cat window.jsonl)"
decodes_cleanly window.mp4 || fail "window.mp4 has decode errors"
pass "closing the recorded window saves $(probe window.mp4 width,height | tr , x) and exits 8"

# Detached start, status, and stop.
started=$("$airec" start --out detached.mp4 --json) || fail "start exited $?"
jq -e '.event == "started"' <<<"$started" >/dev/null || fail "start printed $started"
status=$("$airec" status --json) || fail "status exited $?"
jq -e 'length == 1 and .[0].state == "recording" and (.[0].pipe | startswith("@airec-"))' \
    <<<"$status" >/dev/null || fail "status printed $status"
sleep 2
"$airec" stop --json >stop.jsonl || fail "stop exited $?: $(cat stop.jsonl)"
event stop.jsonl saved | jq -e '.stop_reason == "requested"' >/dev/null ||
    fail "stop printed $(cat stop.jsonl)"
decodes_cleanly detached.mp4 || fail "detached.mp4 has decode errors"
set +e
"$airec" status --json >/dev/null
code=$?
set -e
[ "$code" = 4 ] || fail "status after stop exited $code"
pass "start, status, and stop round trip through the session socket"

# A killed recorder leaves every completed fragment playable.
"$airec" start --out killed.mp4 --json >/dev/null || fail "start before kill exited $?"
sleep 4
pkill -KILL -f '_session --config' || fail "no session process to kill"
sleep 0.5
killed_duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 killed.mp4)
awk -v d="$killed_duration" 'BEGIN { exit !(d >= 2) }' || fail "killed recording lasts $killed_duration"
decodes_cleanly killed.mp4 || fail "killed.mp4 has decode errors"
pass "SIGKILL after 4 s leaves ${killed_duration}s of playable video"

# Two windows play different tones; each recording holds only its own window's tone.
if jq -e '.audio.available' <<<"$doctor" >/dev/null; then
    tone() {
        ffplay -hide_banner -loglevel error -f lavfi -i "sine=frequency=$1:sample_rate=48000" \
            -t 60 -autoexit -showmode 1 -window_title "$2" -x 320 -y 180 &
    }
    tone 440 airec-e2e-low
    low_player=$!
    tone 1000 airec-e2e-high
    high_player=$!
    wait_for_window airec-e2e-low || fail "low tone window never appeared"
    wait_for_window airec-e2e-high || fail "high tone window never appeared"
    sleep 1
    "$airec" record --window airec-e2e-low --window airec-e2e-high --audio --duration 4s \
        --out "audio-{target}.mp4" --json >audio.jsonl || fail "audio record exited $?: $(cat audio.jsonl)"
    kill "$low_player" "$high_player"
    wait "$low_player" "$high_player" 2>/dev/null || true
    low_file=$(ls audio-airec-e2e-low-*.mp4)
    high_file=$(ls audio-airec-e2e-high-*.mp4)
    for file in "$low_file" "$high_file"; do
        [ "$(ffprobe -v error -select_streams a:0 -show_entries stream=codec_name,sample_rate,channels \
            -of csv=p=0 "$file")" = "opus,48000,2" ] || fail "$file has no 48 kHz stereo Opus track"
        decodes_cleanly "$file" || fail "$file has decode errors"
    done
    low_in_low=$(band_level "$low_file" 440)
    high_in_low=$(band_level "$low_file" 1000)
    low_in_high=$(band_level "$high_file" 440)
    high_in_high=$(band_level "$high_file" 1000)
    louder_by "$low_in_low" "$high_in_low" 20 ||
        fail "low window recorded 440 Hz at $low_in_low dB and 1000 Hz at $high_in_low dB"
    louder_by "$high_in_high" "$low_in_high" 20 ||
        fail "high window recorded 1000 Hz at $high_in_high dB and 440 Hz at $low_in_high dB"
    pass "each window's Opus track holds only its own tone ($low_in_low / $high_in_high dB)"
    converted_audio=$("$airec" convert "$low_file" --json) || fail "convert of audio MP4 exited $?"
    jq -e '.event == "converted" and .frames >= 1' <<<"$converted_audio" >/dev/null ||
        fail "convert of audio MP4 printed $converted_audio"
    pass "a recording with audio converts to GIF"
    cp "$low_file" "$high_file" "$evidence/"
else
    echo "skip - window audio, doctor reports $(jq -c .audio <<<"$doctor")"
fi

converted=$("$airec" convert monitor.mp4 --json) || fail "convert exited $?: $converted"
jq -e '.event == "converted" and .width == 960 and .frames >= 1 and .duration_ms >= 4500' \
    <<<"$converted" >/dev/null || fail "convert printed $converted"
[ "$(probe monitor.gif codec_name,width)" = "gif,960" ] || fail "gif stream: $(probe monitor.gif codec_name,width)"
gif_background=$(pixel monitor.gif 3 900 480)
near "$gif_background" "124 58 237" 6 || fail "GIF background decoded as $gif_background"
set +e
"$airec" convert monitor.mp4 --json >/dev/null
code=$?
set -e
[ "$code" = 5 ] || fail "second convert exited $code instead of refusing to overwrite"
pass "convert writes a 960 px GIF with faithful colour and refuses to overwrite it"

ffmpeg -v error -y -ss 1.9 -i monitor.mp4 -frames:v 1 "$evidence/monitor-click.png"
ffmpeg -v error -y -ss 1 -i window.mp4 -frames:v 1 "$evidence/window.png"
cp monitor.mp4 monitor.gif window.mp4 events.jsonl "$evidence/"
echo "evidence in $evidence"
