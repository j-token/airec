# airec v0.1 implementation decisions

This file records PRD ambiguities resolved while preserving the public JSONL and error contracts.

## D-001 — JSONL envelope

Every event has `event`, `ts`, `session`, and `target`. `started` is session-wide, so its `target` is `null` and it additionally has `targets`. Session-wide errors also use `target: null`; target-specific errors contain the output target name. Optional fields are omitted only when they are inapplicable. This resolves the conflict between FR-007's common-field rule and its `started` example.

## D-002 — Detached command event ownership

`start` returns after proxying the child session's `started` event. `record` owns and emits live `heartbeat` events. `stop` proxies terminal `target_lost`, `saved`, and `error` events after finalization. `status` reports active sessions only, as FR-006 specifies. A detached child never inherits the caller's stdout/stderr handles; startup diagnostics are written to a per-session local log and replayed to the `start` caller's stderr before it returns.

## D-003 — Window closure

The public `stop_reason` is always `target_lost`, as defined by FR-007. A `target_lost` event contains `data.cause = "window_closed"`. With `continue`, other targets continue and final process status is `PARTIAL_FAILURE` (6). With `abort`, other targets finalize with `aborted_on_failure` and the process status is `ABORTED_ON_FAILURE` (6). For a single window, closure finalizes the file and exits with capture failure (3), because FR-007 classifies it as unintended.

## D-004 — First-frame timeout

The v0.1 public CLI does not add an undocumented option. The timeout is a core constant of 30 seconds and is injectable through the library session options for tests and future GUI use.

## D-005 — Capture border

airec requests `IsBorderRequired = false`. If the property is unavailable, denied, or ignored, airec writes one warning to stderr and continues with the OS border. It never opens a consent UI.

## D-006 — Resize and frame pacing

The encode resolution is fixed from the first frame. Later frame sizes are aspect-fit into that canvas with black letterboxing. A session clock repeats the last frame at the configured FPS when WGC supplies no new frame, so static recordings retain the requested duration.

## D-007 — Fragmented MP4 and encoder fallback

The encoder requests the Media Foundation `FMPEG4` container subtype rather than regular `MPEG4`, with video-only H.264. Hardware is tried first. On the first evidence sample, airec waits for Media Foundation's second video-sample request (proof that the first sample crossed the asynchronous transcoder) or an asynchronous failure. A failure before that readiness point removes the zero-frame output and retries the same first sample in software, with a stderr warning. After readiness, a failing encoder is never replaced over the same path, so fallback cannot truncate prior video. Failure of both startup paths is `ENCODER_UNAVAILABLE`.

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

WGC frames and `WH_MOUSE_LL` events first use one monotonic epoch created before capture starts. The first delivered video frame establishes the public recording origin. Encoded frames, click compositing, and sidecar `t_ms` values all subtract that same origin; pre-roll input and input after the final video timestamp are omitted.
