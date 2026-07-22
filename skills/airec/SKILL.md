---
name: airec
description: Record Windows screen evidence with the airec CLI, inspect JSONL session state, stop recordings safely, and convert recorded MP4 evidence to GIF. Use when an agent must capture verifiable evidence of its own Windows computer-use work.
---

# airec evidence recording

Use `airec` only for explicit evidence tasks. It records pixels and mouse activity locally; it never needs network access and must not be used to collect keyboard input. Recordings can contain passwords, access tokens, personal data, and other sensitive screen content: review every output before sharing or uploading it. UAC secure-desktop and DRM-protected content may appear black and should not be treated as complete evidence.

## Install the CLI

This skill is instructions only; `airec.exe` is a separate download. If `airec --version` fails, install it before doing anything else:

```powershell
irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1 | iex
```

The script verifies the release checksum, installs to `%LOCALAPPDATA%\airec\bin`, and adds that directory to the user PATH. It needs no administrator rights and no Rust toolchain. PATH changes do not reach an already-running shell, so in the current session invoke the executable by its full path, `$env:LOCALAPPDATA\airec\bin\airec.exe`, until a new shell is started.

Requires Windows 10 version 2004 or later on x86_64. Do not install by building from source unless the user asks for it; a release build takes minutes.

## Choose the failure policy

- Use `--on-failure continue` when evidence from the targets that remain available is still useful. A partial multi-target result exits with code 6, so retain the successful files listed in the error data.
- Use `--on-failure abort` when evidence is valid only if every requested target records completely. Any target failure finalizes the others and exits with code 6.

## Record evidence

1. Run `airec doctor --json`. Do not begin if WGC or both encoders are unavailable.
2. Discover stable targets with `airec list monitors --json` or `airec list windows --json`.
3. For a bounded foreground task, run `airec record --window-handle <HWND> --duration <TIME> --out <FILE>.mp4 --json`.
4. For work whose end is controlled separately, run `airec start ... --json`, retain the `session`, inspect it with `airec status --json`, then run `airec stop --session <ID> --json`.
5. Read stdout as JSONL. Keep diagnostics from stderr separate.
6. Confirm a terminal `saved` event, verify the reported file exists and is non-empty, and inspect `stop_reason` before treating the evidence as successful.

The intended `stop_reason` values are exactly `requested`, `duration_limit`, and `max_duration`. Every other value, including `target_lost`, `error`, and `aborted_on_failure`, is unintended and requires investigation even if a playable file was finalized.

For multiple targets, prefer `--out-dir <DIR>`. If using `--out`, include `{target}` in its filename. Use `--event-log <FILE>.jsonl` only when mouse evidence is needed; airec never captures keyboard events.

## Configure effects

Place `airec.toml` in the working directory for task-specific defaults or in the user home directory for user defaults. The working-directory file wins, and explicit CLI flags win over the selected file. Use `[defaults]` for recording defaults and `[effects]` for click, drag, and movement-trail colors, sizes, durations, and drag thresholds.

## Convert evidence to GIF

Run `airec convert <INPUT>.mp4 --out <OUTPUT>.gif --json`. Optionally set `--fps` and `--width`. Conversion never overwrites an existing output; preserve the source MP4 as the primary evidence.
