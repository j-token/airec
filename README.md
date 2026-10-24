# airec

[한국어](README.ko.md)

A command-line screen recorder for AI agents that need to show their work, on Windows and on Linux under X11.

An agent finishes a computer-use task and you want to know whether it actually worked. Screenshots only show moments. airec records the whole session to MP4, so you can attach the file to a pull request and watch what happened instead of reinstalling the app and clicking through it yourself.

## What it does

- Records a monitor, several monitors at once, or specific application windows, each to its own file
- On Windows, captures a window even when other windows cover it, using Windows Graphics Capture
- Burns click, drag, and pointer-trail effects into the frames so the mouse is visible during playback
- Emits lifecycle events as JSONL on stdout, so a script can tell whether recording started, how it is going, and why it stopped
- Converts a recording to GIF

It has no GUI, no audio capture, no network access, no upload feature, no keyboard hook, and no always-on daemon.

> Recordings can contain passwords, tokens, and personal data. Review one before you share it. UAC secure-desktop and DRM-protected content are not bypassed and will appear black.

## Install

### Windows

Windows 10 version 2004 or later, or Windows 11, on x86_64.

```powershell
irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1 | iex
```

That takes the latest release, checks its SHA256, drops `airec.exe` in `%LOCALAPPDATA%\airec\bin`, and puts that directory on your user PATH. No admin rights, no Rust toolchain, nothing to compile. Open a new terminal afterwards so the PATH change applies.

To pin a version, install elsewhere, or remove it, run the script as a scriptblock so it can take arguments:

```powershell
$s = [scriptblock]::Create((irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1))
& $s -Version v0.2.2
& $s -InstallDir D:\tools\airec
& $s -Uninstall
```

Uninstalling removes the executable and the PATH entry, and leaves your recordings and `airec.toml` alone.

If piping a script into your shell is not something you want to do, take the zip from the [releases page](https://github.com/j-token/airec/releases), verify it against the `.sha256` next to it, and put `airec.exe` anywhere on your PATH.

### Linux

Ubuntu 22.04 LTS or later, or another x86_64 distribution with glibc 2.35 or later, running an X11 session. Wayland sessions are not supported; see [Status and limits](#status-and-limits).

```sh
curl -fsSL https://raw.githubusercontent.com/j-token/airec/main/install.sh | sh
```

That takes the latest release, checks its SHA256, and installs `airec` to `~/.local/bin`. No root, no system packages: the X11 client and the OpenH264 encoder are compiled into the executable. Set `AIREC_VERSION=v0.3.0` to pin a release, `AIREC_INSTALL_DIR` to install elsewhere, or pass `--uninstall` with `curl ... | sh -s -- --uninstall` to remove it.

On a headless server or in a cloud sandbox, give airec a virtual display such as `Xvfb :99 -screen 0 1920x1080x24` and set `DISPLAY=:99`.

### From source

Building from source needs Rust 1.89 or later, and a C compiler on Linux for the bundled OpenH264:

```powershell
cargo build --release
```

The executable lands at `target\release\airec.exe`, or `target/release/airec` on Linux. Confirm capture and encoding work on your machine before you depend on them:

```powershell
airec doctor --json
```

On Windows, `doctor` reports whether Windows Graphics Capture is available, which encoder will be selected, and whether hardware encoding succeeded. On a GPU-less VM it falls back to software encoding and says so. On Linux it reports the `DISPLAY` it connected to, whether the session is Wayland, whether XFixes can supply the cursor image, and the monitors it found; encoding is always the bundled OpenH264 software encoder.

## Recording

Find something to record:

```powershell
airec list monitors --json
airec list windows --json
```

Record the primary monitor for 30 seconds:

```powershell
airec record --duration 30s --out demo.mp4
```

Record every monitor to its own file:

```powershell
airec record --monitor all --duration 30s --out-dir .\rec
```

Record two windows, each to its own file. A window is matched by title substring, by HWND (the X11 window id on Linux), or by process name:

```powershell
airec record --window "Chrome" --window "Windows Terminal" --duration 30s --out-dir .\rec
airec record --window-handle 6758808 --process notepad.exe --duration 30s --out-dir .\rec
```

Output is video-only H.264 in a fragmented MP4 at yuv420p, tagged as BT.709 so every player shows the same colors. It plays in the Windows player, in Chrome, and in the GitHub web player. Fragmented means a killed process still leaves a playable file covering everything recorded up to that point.

## Driving it from an agent

Recording usually has to span a task, so `start` and `stop` are separate commands. `start` detaches and returns once the first frame is confirmed:

```powershell
airec start --window "MyApp" --out evidence.mp4 --json
# {"event":"started","session":"a1b2",...}

# the agent does its work here

airec status --json
airec stop --json
# {"event":"saved","file":"...evidence.mp4","stop_reason":"requested","duration_ms":42180,"frames":2530,...}
```

Every event carries `stop_reason` when it ends a target. Three values mean the recording ended on purpose:

| `stop_reason` | Meaning |
| --- | --- |
| `requested` | `stop` was called |
| `duration_limit` | `--duration` elapsed |
| `max_duration` | the `--max-duration` ceiling was hit |

Anything else means it ended without being asked to: `target_lost` for a closed window, `error` for a pipeline failure, `aborted_on_failure` when another target failed under `--on-failure abort`. Checking that one field is enough to decide whether the evidence is trustworthy.

When you record several targets and one of them fails, you choose what happens:

```powershell
airec record --monitor all --on-failure continue   # default; keep the rest, exit 6 with PARTIAL_FAILURE
airec record --monitor all --on-failure abort      # finalize everything and fail
```

Use `continue` when partial evidence is still worth having, `abort` when only a complete recording counts.

Errors are structured, and the exit code matches the code:

```json
{"event":"error","code":"TARGET_NOT_FOUND","message":"no window matches title 'MyAp'","data":{"query":"MyAp","candidates":[]}}
```

| Code | Exit |
| --- | --- |
| `TARGET_NOT_FOUND`, `AMBIGUOUS_TARGET` | 2 |
| `CAPTURE_INIT_FAILED`, `ENCODER_UNAVAILABLE`, `FIRST_FRAME_TIMEOUT` | 3 |
| `NO_ACTIVE_SESSION`, `SESSION_AMBIGUOUS` | 4 |
| `OUTPUT_IO_ERROR` | 5 |
| `PARTIAL_FAILURE`, `ABORTED_ON_FAILURE` | 6 |
| `PERMISSION_DENIED` | 7 |
| `TARGET_LOST` | 8 |

`AMBIGUOUS_TARGET` returns the candidate windows in `data`, so an agent can narrow the query on its own instead of guessing.

All of this is packaged as an agent skill under `skills/airec`, so the agent starts out knowing the commands and the `stop_reason` rule.

The skill is instructions, not the recorder. Whichever way you install the skill, install the CLI first, or the agent will call a command that does not exist:

```powershell
irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1 | iex
```

```sh
curl -fsSL https://raw.githubusercontent.com/j-token/airec/main/install.sh | sh
```

To install it as a Claude Code plugin, run the following in a shell. Inside a Claude Code session, use `/plugin` in place of `claude plugin`.

```powershell
claude plugin marketplace add j-token/airec
claude plugin install airec@airec
```

To install it as a Codex plugin, run the following, or pick it from `/plugins` inside `codex`. The skill appears in new sessions.

```powershell
codex plugin marketplace add j-token/airec
codex plugin add airec@airec
```

The plugin version matches the release version, so the skill updates when a new release ships. Claude Code updates with `claude plugin update airec@airec`. Codex updates with `codex plugin marketplace upgrade airec` followed by `codex plugin add airec@airec` again.

For other agents, install with the Skills CLI:

```powershell
npx skills add j-token/airec --skill airec
```

Add `-g` to install the skill for every project instead of the current one. Without `--skill airec` the CLI also lists this repository's own workflow skills, which are not part of the recorder.

> Do not run `npx skills add` inside a clone of this repository. The Skills CLI writes the skill to `.agents/skills/<name>` and points the discovery directories at it with symlinks. When a real `skills/<name>` directory is already there it replaces that directory with the symlink, and the files that were in it are gone with no warning and no backup. In this repository `skills/airec` is the skill's source, so installing here destroys it. Elsewhere this only matters if you keep a skill of your own under the same name.

## Pointer effects

Clicks, drags, and pointer movement are drawn into the frames before encoding, so they survive in any player and need no post-processing. Left and right clicks get different colors, and a drag shows the path from where it started.

Defaults are left `#FFD400`, right `#00A2FF` at 34 px for 500 ms, drag `#FF4081` at 4 px for 650 ms, and trail `#00E5FF` at 3 px for 250 ms. A press becomes a drag after it moves 4 px or stays down 150 ms while moving. All of it is adjustable:

```powershell
airec record --drag-color "#FF00AA" --drag-size 6 --trail-duration-ms 400 --duration 30s --out demo.mp4
```

`--no-effects` turns the drawing off, `--no-cursor` removes the OS cursor from the capture. Only the mouse is hooked. Keyboard input is never captured or logged.

`--event-log events.jsonl` additionally writes a mouse timeline with video-relative timestamps, so an agent can cross-check "what was clicked at 12.4 s" as text instead of watching the video.

## Converting to GIF

```powershell
airec convert demo.mp4 --fps 10 --width 960
```

Decoding and GIF writing happen in-process, through Media Foundation on Windows and the bundled OpenH264 decoder on Linux. Nothing is downloaded and no ffmpeg install is required. Defaults are 10 fps and a maximum width of 960 px, aspect ratio preserved, and smaller inputs are not enlarged. An existing output is never overwritten.

Frames after the first are written as differences against the previous frame, so a recording where little moves stays much smaller than one where the whole screen changes. A 3-second 1080p desktop recording measured 533 KB as MP4 and 501 KB as GIF at 960 px wide. Content that changes on every frame, such as scrolling or video playback, gets little benefit from that and can still produce a GIF several times the size of the source.

GIF is for pasting somewhere that will not play video. It is not a way to save space.

WebM is not supported. Windows has no consistently available built-in WebM encoder, and requiring users to install a codec or ffmpeg would defeat the point of shipping one executable.

## Configuration

Defaults can live in `airec.toml`, read from the current directory, or from `%USERPROFILE%` (`$HOME` on Linux) if there is none:

```toml
[defaults]
fps = 30
quality = "medium"
effects = true

[effects]
click_color_left = "#FFD400"
click_color_right = "#00A2FF"
```

An explicit flag beats the config file, which beats the built-in default. A config file that exists but is invalid is an error rather than a silent fallback to the home directory.

## Status and limits

Linux support covers X11 sessions only. Ubuntu 22.04 and 24.04 desktops start in Wayland by default; pick "Ubuntu on Xorg" on the login screen to use airec there. Under Wayland, screen capture goes through an xdg-desktop-portal dialog where a person has to pick the target, and global mouse input is not available to applications at all, so an agent cannot record unattended. `doctor` reports `wayland_session: true` in that case. airec may still connect through XWayland, but XWayland only holds X11 applications, so native Wayland windows and the desktop are missing from what it records.

On Linux, window capture reads the window's pixels from the X server. Without a compositing window manager, the covered part of a window shows whatever is on top of it, unlike Windows Graphics Capture. Monitor capture is unaffected.

macOS through ScreenCaptureKit is planned, with a first-run permission step.

Also not present: audio, region capture, keyboard display, streaming, editing, and automatic upload. Upload stays out on purpose, since `gh` and similar tools already do it and airec has no business touching the network.

A Tauri v2 and Svelte GUI is planned for v0.4 on top of the same `airec-core` crate the CLI uses.

## License

Apache License 2.0. See [LICENSE](LICENSE).

`vendor/windows-capture` is a modified copy of the MIT-licensed [windows-capture](https://github.com/NiiightmareXD/windows-capture) crate and stays under its own license. [NOTICE](NOTICE) lists the attribution, and `DECISIONS.md` entry D-015 records what was changed and why.
