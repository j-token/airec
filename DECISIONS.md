# airec v0.1 implementation decisions

This file records PRD ambiguities resolved while preserving the public JSONL and error contracts.

## D-001 — JSONL envelope

Every event has `event`, `ts`, `session`, and `target`. `started` is session-wide, so its `target` is `null` and it additionally has `targets`. Session-wide errors also use `target: null`; target-specific errors contain the output target name. Optional fields are omitted only when they are inapplicable. This resolves the conflict between FR-007's common-field rule and its `started` example.

## D-002 — Detached command event ownership

`start` returns after proxying the child session's `started` event. `record` owns and emits live `heartbeat` events. `stop` proxies terminal `target_lost`, `saved`, and `error` events after finalization. `status` reports active sessions only, as FR-006 specifies. `start` creates the detached child with `CreateProcessW` and `bInheritHandles = FALSE`, so it cannot retain the caller's stdout/stderr pipe handles after the parent exits. Startup diagnostics are written to a per-session local log and replayed to the `start` caller's stderr before it returns.

## D-003 — Window closure

The public `stop_reason` is always `target_lost`, as defined by FR-007. A `target_lost` event contains `data.cause = "window_closed"`. With `continue`, other targets continue and final process status is `PARTIAL_FAILURE` (6). With `abort`, other targets finalize with `aborted_on_failure` and the process status is `ABORTED_ON_FAILURE` (6). For a single window, closure finalizes the file and exits with capture failure (3), because FR-007 classifies it as unintended.

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
