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
