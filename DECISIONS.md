# airec v0.1 implementation decisions

This file records PRD ambiguities resolved while preserving the public JSONL and error contracts.

## D-001 — JSONL envelope

Every event has `event`, `ts`, `session`, and `target`. `started` is session-wide, so its `target` is `null` and it additionally has `targets`. Session-wide errors also use `target: null`; target-specific errors contain the output target name. Optional fields are omitted only when they are inapplicable. This resolves the conflict between FR-007's common-field rule and its `started` example.

## D-002 — Detached command event ownership

`start` returns after proxying the child session's `started` event. `record` owns and emits live `heartbeat` events. `stop` proxies terminal `target_lost`, `saved`, and `error` events after finalization. `status` reports active sessions only, as FR-006 specifies. `start` creates the detached child with `CreateProcessW` and `bInheritHandles = FALSE`, so it cannot retain the caller's stdout/stderr pipe handles after the parent exits. Startup diagnostics are written to a per-session local log and replayed to the `start` caller's stderr before it returns.

## D-003 — Window closure

The public `stop_reason` is always `target_lost`, as defined by FR-007. A `target_lost` event contains `data.cause = "window_closed"` and takes priority over a concurrent requested or duration stop. With `continue`, other targets continue and final process status is `PARTIAL_FAILURE` (6). With `abort`, other targets finalize with `aborted_on_failure` and the process status is `ABORTED_ON_FAILURE` (6). For a single window, closure after the first frame finalizes the file and exits with `TARGET_LOST` (8); closure before the first frame remains `CAPTURE_INIT_FAILED` (3), because recording did not start.

## D-004 — First-frame timeout

The v0.1 public CLI does not add an undocumented option. The timeout is a core constant of 30 seconds and is injectable through the library session options for tests and future GUI use.

## D-005 — Capture border

airec requests `IsBorderRequired = false`. If the property is unavailable, denied, or ignored, airec writes one warning to stderr and continues with the OS border. It never opens a consent UI.

## D-006 — Resize and frame pacing

The encode resolution is fixed from the first frame. Later frame sizes are aspect-fit into that canvas with black letterboxing. A session clock repeats the last frame at the configured FPS when WGC supplies no new frame, so static recordings retain the requested duration.

## D-007 — Fragmented MP4 and encoder fallback

The encoder requests the Media Foundation `FMPEG4` container subtype rather than regular `MPEG4`, with video-only H.264. Before opening evidence files, airec completes a one-frame 1280×720 hardware transcode and waits for Media Foundation's second video-sample request, proving that the first sample crossed the asynchronous transcoder. The process caches that selection for all targets in the session. A failed hardware probe selects a similarly verified software path and emits a stderr warning. A synchronous failure while opening the real evidence encoder can still fall back only before any evidence frame is committed. After recording begins, an encoder is never replaced over the same path, so fallback cannot truncate prior video. Failure of both startup paths is `ENCODER_UNAVAILABLE`.

## D-008 — Input coordinate space

The process opts into Per-Monitor-V2 DPI awareness. Mouse-hook screen coordinates and window client rectangles are converted to the physical-pixel WGC frame space at the frame timestamp. Non-client and outside-window clicks are excluded.

## D-009 — Session discovery

Named pipes remain the control transport (`\\.\pipe\airec-<session-id>`). Small local state files under the user's temporary directory are only a discovery index and crash-recovery status cache; they contain session metadata and output paths, never captured pixels or input beyond an explicitly requested event log. Normal `stop` removes launch, state, and diagnostic metadata; discovery prunes completed entries. Crash remnants are retained as recovery evidence but are never treated as active without a reachable pipe.

## D-010 — Odd capture dimensions

H.264 4:2:0 encoders require even dimensions. If a monitor or window has an odd physical dimension, the fixed session canvas is expanded by one black pixel on that axis. The captured content is not cropped or distorted.

## D-011 — Multi-target output templates

For multi-target capture, `--out` is a template only when it contains the literal `{target}` placeholder. The placeholder expands to the stable target id, such as `monitor-2` or `my-app-1a2b`. A multi-target `--out` without that placeholder is `OUTPUT_IO_ERROR`; `--out-dir` remains the simpler alternative.

## D-012 — CLI validation in JSON mode

The PRD has no separate invalid-arguments error code. When clap validation fails and `--json` is present, airec emits one `CAPTURE_INIT_FAILED` event with `data.component = "cli"` and exits 3. Help and version remain ordinary successful text output. This keeps every machine-mode error inside the published §13 code set.

## D-013 — Recording timeline origin

WGC frames and `WH_MOUSE_LL` events first use one monotonic epoch created before capture starts. All target pipelines rendezvous after their first-frame attempt; the earliest successful first frame establishes the session origin. Every encoder preserves that absolute session timestamp instead of independently rebasing its first sample. Encoded frames, click compositing, and sidecar `t_ms` values therefore share one timeline even when targets start at different times; pre-roll input and input after the final video timestamp are omitted. `started` is published only after each successful pipeline's first sample crosses asynchronous encoder startup, so fallback diagnostics are available to the detached caller.

## D-014 — Doctor encoder probe

`doctor` performs complete one-frame 1280×720 FMPEG4 transcodes with hardware acceleration enabled and disabled. It reports both actual availability results and the selected mode. Temporary probe files are removed; if neither mode succeeds, the command returns `ENCODER_UNAVAILABLE` (exit 3).

## D-015 — Vendored windows-capture fork

The workspace pins `windows-capture` 2.0.0 to `vendor/windows-capture` because the upstream 2.0.0 public encoder exposes the regular MPEG4 container but not Media Foundation's FMPEG4 fragmented container required by FR-006. The fork retains the upstream MIT `LICENCE` and changes `src/encoder.rs` to add the FMPEG4 container subtype, asynchronous transcoder error/readiness tracking, `wait_until_ready`, and `send_frame_buffer_at_timeline` so all target files preserve the shared session timeline; its stream-capture example is adjusted for the changed encoder surface.

Upstream updates are tracked deliberately rather than accepted through an unconstrained Cargo upgrade. Before changing the pinned version, maintainers must compare the vendored tree with the matching upstream release, rebase this minimal fork, document any changed divergence here, retain upstream licensing, and rerun the encoder fallback/readiness tests plus live fragmented-MP4 forced-kill and multi-target timeline checks.

## D-016 — v0.2 effect styles and drag recognition

Effect configuration is a front-end-neutral `airec-core` value using RGB colors, output-frame pixels, and milliseconds. Hex colors use the exact `#RRGGBB` form. The v0.1 click defaults remain left `#FFD400`, right `#00A2FF`, size 34 px, and 500 ms. New defaults are drag `#FF4081`, 4 px, 650 ms and trail `#00E5FF`, 3 px, 250 ms. A drag starts after movement exceeds 4 px or a non-stationary press lasts 150 ms; every mouse move participates in recognition while published move events remain throttled to 100 ms. The timeline retains at most 65,536 events (over 100 minutes at the default move cadence), preventing unbounded session memory growth while retaining a complete default 30-minute sidecar window.

## D-017 — Config selection, precedence, and errors

airec loads at most one config: `airec.toml` in the current directory, otherwise `airec.toml` in `USERPROFILE`. A present but invalid current-directory file is an error and never falls through to home. Merge order is explicit CLI value, selected config value, then the v0.1 default. Unknown keys, invalid values, and TOML syntax errors map to the existing `CAPTURE_INIT_FAILED` code with `data.component = "config"`; file access failures map to `OUTPUT_IO_ERROR`. This keeps the v0.1 error-code set stable.

## D-018 — GIF conversion and WebM deferral

`airec convert` decodes the recorded MP4 through Windows Media Foundation and writes GIF89a internally, with no process execution, runtime download, or external codec. The CLI defaults to 10 FPS and a 960 px maximum width, samples at the requested FPS, preserves aspect ratio without upscaling, writes a sibling temporary file, and only renames after successful finalization. Existing outputs are never overwritten. Decode/format failures map to `CAPTURE_INIT_FAILED`; destination I/O maps to `OUTPUT_IO_ERROR`. WebM is deferred because Windows does not provide a consistently available built-in WebM encoder and adding or requiring a codec/ffmpeg stack would violate the offline and no-native-install constraints.

## D-019 — Window geometry and close notification

Window client clipping and extended-frame mapping are refreshed while recording so move and resize changes follow the captured window. The most recent valid geometry may be reused across a transient query failure, but a confirmed WGC close or invalid HWND always wins and produces `target_lost`. The WGC close callback stores an atomic closed state before making a non-blocking channel notification, so a saturated frame queue cannot delay target-loss detection.

## D-020 — Claude Code skill artifact

The requested distributable artifact root is `skill/`, with `skill/SKILL.md` carrying valid skill frontmatter. Installers copy that directory as the `airec` skill into the Claude Code user or project skill location; the repository does not duplicate it under `.claude`, avoiding two independently drifting instructions. Automated validation checks the package structure; discovery and triggering remain a manual Claude Code integration check.

## D-021 — Fidelity-preserving GIF frame differencing

GIF conversion keeps the requested sampling rate and output dimensions, but no longer emits every sampled frame as a full opaque image. The first image and changed colors use deterministic 5-bit histogram median-cut palettes with up to 255 opaque colors. Later images reserve index zero for transparency, use disposal method 1 (`do not dispose`), crop to the union rectangle of pixels that improve the composited image's squared RGB error, and preserve all other pixels from the previous canvas. Identical sampled frames are represented by extending the pending image delay. Delays remain derived from the source timeline, so descriptor collapse does not drop elapsed time or change playback speed.

The GIF retains the legacy 3-3-2 palette as its global table. This gives high-change frames a compatibility and size fallback: the encoder compares actual LZW sizes and may write a full fixed-palette image when it is smaller than the adaptive difference. That fallback exactly matches the old quantization error. Adaptive updates are accepted only when the resulting full-canvas MSE is no worse than the old 3-3-2 result. A trial 63-color first-frame limit was rejected after visible color shifts despite a favorable aggregate MSE; the default uses 255 colors and does not dither because dithering adds flat-region noise and harms LZW compression.

The deterministic 31-frame, 960×540, 10 FPS report benchmark runs with:

`cargo test --release -p airec-capture report_default_dimension_gif_benchmark -- --ignored --nocapture`

| Scenario | Legacy bytes | Adaptive full-frame bytes | Optimized bytes | Reduction | Legacy RGB MSE | Optimized RGB MSE |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Near-static window and moving cursor | 2,515,275 | 2,749,165 | 119,604 | 95.24% | 477.2956 | 37.6046 |
| Localized text change | 2,525,175 | 2,756,784 | 105,355 | 95.83% | 477.4084 | 37.5129 |
| Large-area scroll | 204,321 | 461,983 | 212,771 | -4.14% | 547.7675 | 530.1520 |
| Full-frame change | 21,185,736 | 21,413,782 | 21,192,892 | -0.03% | 424.4797 | 416.0086 |

The near-static evidence case clears both the required 90% and stretch 95% reductions. Localized change also clears both. Scroll and full-frame change deliberately do not trade fidelity for the size target; the fixed-palette fallback limits their growth while retaining slightly lower error. Tests independently parse LZW data, local palettes, transparency, and the composited canvas. A release conversion was also decoded as an 11-frame 960×544 GIF by Windows WIC and rendered by Chrome headless. GIF89a, Netscape looping, temporary-output commit, overwrite refusal, error mapping, dimensions, sampling rate, and existing `converted` JSONL fields remain unchanged.

## D-022 — Skills CLI distribution layout

The skill package moved from `skill/` to `skills/airec/`, superseding the location chosen in D-020. The Skills CLI (`npx skills`) discovers skills by walking a fixed set of container directories one level deep; `skills/` is on that list and `skill/` is not, so the previous layout was only reachable through the CLI's fallback recursive search. The CLI also requires the frontmatter `name` to match the parent directory name, which `skill/` violated while declaring `name: airec`.

The directory name is therefore the installed skill name, and `npx skills add j-token/airec --skill airec` is the supported install path. No manifest file is involved; the Skills CLI has no publish or registry step and resolves everything from the git repository layout.

The repository's own agent skills under `.agents/skills` remain where they are. Those paths are also CLI discovery locations, so a bare `npx skills add j-token/airec` lists them alongside `airec`; installing the recorder skill alone requires the `--skill airec` filter.

## D-023 — Prebuilt release distribution and install script

Building from source was the only install path, which required a Rust 1.85 toolchain and minutes of compilation for a tool whose users mostly want one executable. Releases are now built by GitHub Actions on a `v*` tag push, producing a single `airec-<tag>-x86_64-pc-windows-msvc.zip` containing `airec.exe`, `LICENSE`, `NOTICE`, and `README.md`, plus a sibling `.sha256`. The workflow refuses to build when the tag does not match the workspace version in `Cargo.toml`, so a release can never disagree with what `airec --version` reports, and it builds with `--locked` so the published binary matches the committed `Cargo.lock`.

`install.ps1` at the repository root is the supported install path, run as `irm .../install.ps1 | iex`. It resolves a release through the GitHub API, verifies the SHA256 before extracting anything, installs to `%LOCALAPPDATA%\airec\bin`, and appends that directory to the user PATH. It needs no administrator rights, which rules out per-machine locations. Arguments are reachable by constructing a scriptblock, since a piped script cannot take parameters: `-Version` pins a tag, `-InstallDir` relocates, `-Uninstall` removes the executable and the PATH entry while leaving recordings and `airec.toml` in place.

The user PATH is read and written through `HKCU:\Environment` rather than `[Environment]::SetEnvironmentVariable`, which rewrites the value as `REG_SZ` and would permanently expand any `%VAR%` entries the user already had. The existing value kind is preserved rather than normalized, and the registry write is followed by a `WM_SETTINGCHANGE` broadcast, which the .NET API would have sent on its own; without it, processes launched from Explorer keep the stale environment until sign-out. PATH is written only when it actually changes.

Package managers were not used. winget and Scoop both want a published manifest in a third-party repository, which adds a review-gated step between tagging and users being able to install, and neither removes the need for a release asset the manifest points at. `cargo install --git` remains possible for Rust users but is not documented as the primary path, because it reintroduces the toolchain and compile time this decision exists to remove. Only x86_64 is published; the script rejects other architectures rather than installing a binary that cannot run, and ARM64 is accepted because it runs x86_64 under emulation.

## D-024 — Color-faithful MP4 and GIF output

Recordings looked shifted or banded depending on where they were viewed, and three separate causes were measured on a window showing known sRGB patches and gradients.

MP4 frames were handed to Media Foundation as BGRA, and the transcoder chose the YUV matrix from the frame size: BT.601 for a 980×560 window, BT.709 for a 1600×900 one, ignoring any requested matrix. The stream carried no color description, so players had to guess, and they guess differently. ffmpeg-based players decode untagged H.264 as BT.601, which shifted saturated colors in HD recordings by up to 25 code values. airec now converts BGRA to NV12 itself with the BT.709 limited-range matrix, averaging each 2×2 block for chroma, and tags the stream with BT.709 matrix, primaries, transfer, and limited range. Tagging only the matrix as BT.601 was rejected because Media Foundation's decoder falls back to BT.709 whenever the primaries are BT.709, and size-dependent tags were rejected because the transcoder's size threshold is undocumented. Measured patch error is now at most 3 in ffmpeg, Chromium, and the Media Foundation decoder at both sizes.

GIF conversion read the decoded frame size without the display aperture. H.264 is coded in 16-pixel macroblocks, so a 980-pixel-wide recording decoded as 992 pixels, with the padding shown as a green stripe and the picture squeezed horizontally. Frames are now cropped to `MF_MT_MINIMUM_DISPLAY_APERTURE`.

The fixed 3-3-2 palette from D-021 is removed. It was chosen whenever its LZW stream was smaller, and adaptive updates were accepted whenever they were no worse than it, so scrolls and gradients degraded to 8 levels of red and green and 4 of blue, and dark grays such as `#141414` rendered black. The first frame's adaptive palette is now the global color table. Each difference frame is mapped onto both that palette and a fresh adaptive palette of its changed pixels, and whichever leaves less squared RGB error over the changed pixels is written. Error is no longer traded for size. The median-cut histogram also moved from 5 to 6 bits per channel, because 32 levels left visible steps in gray gradients; this tripled the synthetic benchmark's run time but added about 10% to the conversion of a real 6-second recording.

| Scenario (D-021 benchmark) | D-021 bytes | Now bytes | D-021 RGB MSE | Now RGB MSE |
| --- | ---: | ---: | ---: | ---: |
| Near-static window and moving cursor | 119,604 | 100,854 | 37.6046 | 35.9447 |
| Localized text change | 105,355 | 88,818 | 37.5129 | 36.0191 |
| Large-area scroll | 212,771 | 588,062 | 530.1520 | 0.4020 |
| Full-frame change | 21,192,892 | 21,452,518 | 416.0086 | 159.2880 |

On the recorded 1600×900 test window, the static region of a full-size GIF rose from 23.6 dB to over 43 dB PSNR against its MP4.

## D-025 — Claude Code and Codex plugin distribution

The skill is also published as a Claude Code plugin and a Codex plugin, so those agents install it through their own plugin commands instead of the Skills CLI. The repository root is both the plugin and its single-plugin marketplace for each agent. Claude Code reads `.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json`, whose entry has source `"./"`. Codex reads `.codex-plugin/plugin.json` and `.agents/plugins/marketplace.json`, whose entry has the local source path `"./"`. Both marketplaces and the plugin are named `airec`, so the install id is `airec@airec` in both agents. `skills/airec/` stays the only copy of the skill, which keeps D-022's Skills CLI path working unchanged.

Codex also reads the Claude manifests as a fallback, but its documented marketplace entry carries `policy` and `category`, and its manifest carries `interface` display metadata that Claude Code does not read. Each agent therefore gets the manifests its own documentation describes rather than one shared pair.

Claude Code auto-discovers skills only under the plugin root's `skills/` directory, and the Codex manifest names `./skills/` explicitly. The repository's own workflow skills under `.agents/skills` are copied into the plugin cache along with the rest of the repository but are not loaded; `claude plugin details` lists `airec` as the only component.

Both manifests carry the workspace version, and the release workflow refuses a tag that disagrees with either of them. A declared version means installed copies update only when it changes, so the skill users receive always matches a published CLI release rather than whatever was last merged. Omitting the version was rejected for that reason: Claude Code would then treat every commit as an update and could ship instructions for flags that no released `airec.exe` has.

## D-026 — Release workflow bumps the version

With D-025 a release needed the same version written by hand in three files plus `Cargo.lock` before tagging. The release workflow's manual run now takes a `version` input and does that itself. A `bump` job checks out `main`, rewrites the workspace version in `Cargo.toml`, runs `cargo update --workspace` so only the workspace crates move in `Cargo.lock`, sets `version` in both plugin manifests, commits `misc: 버전을 <version>(으)로 올림` as `github-actions[bot]`, and pushes the commit to `main` together with the tag `v<version>`. The `release` job then builds that tag. It refuses input that is not semver or whose tag already exists.

`Cargo.lock` is updated by Cargo rather than by text replacement, because third-party packages can share the old version string. A tag pushed with `GITHUB_TOKEN` does not start another workflow run, so the jobs are chained with `needs` instead of relying on the tag push. Pushing a hand-made `v*` tag still releases it, with the same consistency check. The direct push to `main` depends on `main` having no protection rule that blocks `github-actions[bot]`; adding one later means switching the bump to a pull request or a token allowed to bypass it.

## D-027 — Linux support through X11

Cloud end-to-end runs need airec on Linux, so v0.3 adds Ubuntu 22.04 LTS and later on x86_64. The capture, input, encoding, decoding, process, and IPC code each moved behind a per-platform module with the same interface; `airec-core`, the effects compositor, the JSONL contract, and the error codes are shared unchanged, and so is every exit code.

Only X11 is supported. The Wayland screencast portal makes a person pick the capture target in a dialog, and Wayland gives no application a global view of the pointer, so an agent could not start a recording unattended, which is the point of airec. A Wayland session is reported by `doctor` as `wayland_session` instead of being worked around. The X11 client is `x11rb` in pure Rust, so the executable links against nothing beyond glibc.

- Capture uses `GetImage` on the root window for monitors and on the window itself for windows. Monitors come from RandR 1.5 and fall back to the whole root screen when RandR has no monitor list, as under a bare Xvfb. Windows come from the EWMH `_NET_CLIENT_LIST` and fall back to mapped children of the root when no window manager publishes one. Without a compositing manager the covered part of a window reads back as whatever covers it; monitor capture is unaffected. `BadWindow` or `BadDrawable` during capture is `target_lost` with `window_closed`, preserving D-003. The cursor is drawn from XFixes, since `GetImage` never contains it.
- Mouse input uses the RECORD extension. It hands every device event to airec together with the root position at the time of the event. XInput2 raw events were tried first and rejected: they carry no screen position, and reading the pointer when each one is processed placed a press and a drag injected in one burst by `xdotool` at the final pointer position, so the drag was logged as a click at the wrong place. X11 has no system-wide double-click time, so the Windows default of 500 ms is used.
- Encoding is OpenH264 compiled from source into the executable, because Ubuntu ships no H.264 encoder that every installation has and D-018 rules out depending on ffmpeg. It runs in screen-content real-time mode with bitrate rate control, an IDR frame every second, and the BT.709 limited-range tags from D-024. Adaptive quantization and background detection are turned off because that mode does not support them.
- OpenH264 emits a raw Annex B stream, so airec writes the fragmented MP4 itself: one fragment per IDR frame with a 90 kHz timescale, which keeps D-007's guarantee that a killed recorder leaves every completed second playable. Frame times are snapped to the 1/fps grid, keeping each frame on its own slot; capture jitter otherwise put two frames inside one frame interval, which ffmpeg 4.4 on Ubuntu 22.04 reports as non-monotonic timestamps.
- `convert` decodes with the same bundled OpenH264 and feeds the shared GIF writer, so GIF output is identical in behavior to D-021 and D-024.
- Session control moves from named pipes to abstract Unix sockets named `@airec-<session>`, which vanish with their process and need no file cleanup. `start` detaches the session with `setsid` and points its stderr at the per-session diagnostic log, matching D-002. `airec.toml` falls back to `$HOME`.

OpenH264's `wide` and `safe_arch` dependencies require Rust 1.89, so the workspace `rust-version` moves from 1.85 to 1.89. Every `wide` 1.x release has the same requirement.

The release workflow adds an `airec-<tag>-x86_64-unknown-linux-gnu.tar.gz` with a sibling `.sha256`, built inside an Ubuntu 22.04 container so it runs on the oldest glibc supported. `install.sh` mirrors `install.ps1`: it verifies the checksum and installs to `~/.local/bin` without root.

`scripts/e2e-linux.sh` exercises the release binary against a live display and checks doctor output, monitor listing, a 5-second recording's codec, size, color tags, duration and clean decode, the recorded background color, pointer and click compositing, the click and drag event log, `target_lost` on window close, detached `start`/`status`/`stop`, a playable file after `SIGKILL`, and GIF conversion. It passed on Ubuntu 24.04 with Xorg and Openbox, and on Ubuntu 22.04 with Xvfb and Openbox, both in Boat sandboxes. The Windows build is type-checked from Linux; its runtime behavior is unchanged.

## D-028 — Window audio

v0.5 adds `--audio` (and `--no-audio`, plus `audio` under `[defaults]`, off by default). It records only the sound that the application owning each window target plays, so a recording of one window never picks up a notification sound, a second browser, or another recorded window. Each window's MP4 gets its own audio track. A monitor has no owning application, and recording everything the system plays would defeat that isolation, so `--audio` with a monitor target fails before recording with `CAPTURE_INIT_FAILED` and `data.component` `audio` instead of silently recording no sound. The owning application is the window's process and all of its descendants, because browsers and Electron apps play audio from a child process.

- Windows uses WASAPI process loopback (`PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`, available from Windows 10 version 2004, the minimum airec already requires) through the `wasapi` crate, asking for 48 kHz 16-bit stereo with automatic conversion. Audio is encoded as 128 kb/s AAC by the Media Foundation sink writer that already writes the video, so the vendored fork from D-015 is used as is.
- Linux talks the PulseAudio native protocol in pure Rust (`pulseaudio` crate), which PipeWire also serves through `pipewire-pulse`, so no client library is linked. Every half second it lists the playback streams (sink inputs) whose `application.process.id` belongs to the window's `_NET_WM_PID` tree, and records each one directly with a record stream on its sink's monitor and `direct_on_input`, the mechanism `parec --monitor-stream` uses. A window without `_NET_WM_PID` fails the recording up front. Audio is encoded as 128 kb/s Opus with libopus built from source, since Ubuntu has no AAC encoder every installation carries; this adds CMake to the build requirements. The fragmented MP4 writer from D-027 gains an `Opus`/`dOps` track, interleaved into the same per-second fragments as the video, so D-007's kill-safety still holds.

Audio follows the D-013 timeline. The track starts at the session's timeline origin and is gapless: time when the application plays nothing is written as silence, so audio and video stay aligned however playback starts and stops. Capture APIs deliver packets late and in bursts, so audio is written 250 ms behind the newest video frame, and on stop the session waits that long once more before finishing, so the end of the track is not cut off. Packets of one stream are laid end to end while their capture time agrees within 80 ms and restart at their capture time beyond that, which absorbs delivery jitter without accumulating drift. Several streams of one application are mixed by summing and clipping. Audio that still arrives after its time was written is dropped. `convert` keeps writing a silent GIF.

`doctor` reports an `audio` object with `available`, `capture`, `codec`, and `error`; missing audio does not change its exit code, since audio is optional.

Verified on Windows 11 with two `ffplay` windows playing 440 Hz and 1000 Hz recorded at once: each file carries only its own tone (the other tone is at least 30 dB down), a detached session killed with `taskkill /F` plays back with audio, and GIF conversion works. `scripts/e2e-linux.sh` gained the same two-window check and passed on Ubuntu 22.04 with Xvfb, Openbox, and PulseAudio's null sink in Docker; the other window's tone is more than 50 dB down, which is why the check measures each band through two narrow bandpass stages rather than one, since one stage passes a tone an octave away at only 17 dB down. Audio started by an application after recording began is picked up within half a second on Linux.
