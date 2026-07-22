use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use airec_capture::{WindowsCaptureBackend, WindowsEncoderFactory, diagnose_encoders};
use airec_core::{
    AirecError, CaptureBackend, CaptureFrame, CaptureTarget, ControlRequest, ControlResponse,
    EffectColor, EffectStyles, EncoderFactory, ErrorCode, Event, FailurePolicy, FrameProcessor,
    InputEvent, InputOptions, InputSource, Pipeline, Quality, RecordingOptions, ResolvedTarget,
    SessionOutcome, SessionRunOptions, SessionSnapshot, SessionState, StopReason, TargetCatalog,
    TargetKind, TargetSnapshot, Timestamp, run_session,
};
use airec_effects::{RippleCompositor, ScreenRect, letterbox};
use airec_input::{EventLogWriter, MouseHook, MouseSubscription};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{ClientToScreen, GetMonitorInfoW, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows_capture::graphics_capture_api::GraphicsCaptureApi;
use windows_capture::monitor::Monitor;

use crate::args::{
    Cli, Command, FailureArg, JsonArgs, ListKind, QualityArg, RecordArgs, RecordingArgs, StartArgs,
    TargetArgs,
};
use crate::ipc;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LaunchConfig {
    session: String,
    targets: Vec<ResolvedTarget>,
    fps: u32,
    quality: Quality,
    cursor: bool,
    effects: bool,
    effect_styles: EffectStyles,
    input: InputOptions,
    max_duration_ms: u64,
    failure_policy: FailurePolicy,
    event_log: Option<PathBuf>,
    verbose: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StateFile {
    snapshot: SessionSnapshot,
    events: Vec<Event>,
    exit_code: Option<i32>,
}

struct SharedState {
    path: PathBuf,
    value: Mutex<StateFile>,
    response_sent: AtomicBool,
}

pub fn execute(cli: Cli) -> Result<i32, (AirecError, bool)> {
    match cli.command {
        Command::List(args) => list(args.kind),
        Command::Record(args) => record(args),
        Command::Start(args) => start(args),
        Command::Status(args) => status(args),
        Command::Stop(args) => stop(args),
        Command::Doctor(args) => doctor(args),
        Command::Convert(args) => convert(args),
        Command::Session(args) => session_child(&args.config).map_err(|error| (error, false)),
        #[cfg(debug_assertions)]
        Command::TestHold(args) => test_hold(args).map_err(|error| (error, false)),
    }
}

fn list(kind: ListKind) -> Result<i32, (AirecError, bool)> {
    let backend = WindowsCaptureBackend::new();
    match kind {
        ListKind::Monitors(args) => {
            let monitors = backend.monitors().map_err(|error| (error, args.json))?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&monitors).expect("monitor JSON serialization")
                );
            } else {
                for monitor in monitors {
                    println!(
                        "{}: {} {}x{}{}",
                        monitor.index,
                        monitor.name,
                        monitor.width,
                        monitor.height,
                        if monitor.primary { " primary" } else { "" }
                    );
                }
            }
        }
        ListKind::Windows(args) => {
            let windows = backend.windows().map_err(|error| (error, args.json))?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string(&windows).expect("window JSON serialization")
                );
            } else {
                for window in windows {
                    println!(
                        "0x{:x} {} [{}] {}x{}",
                        window.hwnd, window.title, window.process, window.width, window.height
                    );
                }
            }
        }
    }
    Ok(0)
}

fn record(args: RecordArgs) -> Result<i32, (AirecError, bool)> {
    let duration = args
        .duration
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|error| (error, args.json))?;
    let (targets, options) =
        prepare(&args.targets, &args.recording).map_err(|error| (error, args.json))?;
    let session = short_session_id();
    let stop = Arc::new(Mutex::new(None));
    let ctrl_stop = stop.clone();
    ctrlc::set_handler(move || {
        *ctrl_stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(StopReason::Requested);
    })
    .map_err(|error| {
        (
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                error.to_string(),
                serde_json::json!({"component": "ctrl_c"}),
            ),
            args.json,
        )
    })?;
    run_live(session, targets, options, duration, stop, args.json, None)
        .map_err(|error| (error, args.json))
}

fn start(args: StartArgs) -> Result<i32, (AirecError, bool)> {
    #[cfg(debug_assertions)]
    if let Some(ready) = std::env::var_os("AIREC_TEST_DETACHED_PIPE_PROBE") {
        return start_detached_pipe_probe(PathBuf::from(ready), args.json);
    }

    let (targets, options) =
        prepare(&args.targets, &args.recording).map_err(|error| (error, args.json))?;
    let session = short_session_id();
    let launch = LaunchConfig {
        session: session.clone(),
        targets,
        fps: options.fps,
        quality: options.quality,
        cursor: options.cursor,
        effects: options.effects,
        effect_styles: options.effect_styles,
        input: options.input,
        max_duration_ms: options.max_duration.as_millis() as u64,
        failure_policy: options.failure_policy,
        event_log: options.event_log,
        verbose: args.recording.verbose,
    };
    let directory = session_directory().map_err(|error| (error, args.json))?;
    let config_path = directory.join(format!("launch-{session}.json"));
    write_json_file(&config_path, &launch).map_err(|error| (error, args.json))?;
    spawn_detached(&config_path).map_err(|error| (error, args.json))?;

    let state_path = state_path(&session).map_err(|error| (error, args.json))?;
    let deadline = Instant::now() + airec_core::FIRST_FRAME_TIMEOUT + Duration::from_secs(2);
    loop {
        if let Ok(state) = read_json_file::<StateFile>(&state_path) {
            if let Some(started) = state
                .events
                .iter()
                .find(|event| matches!(event, Event::Started { .. }))
            {
                print_detached_diagnostics(&session);
                print_event(started, args.json);
                return Ok(0);
            }
            if matches!(
                state.snapshot.state,
                SessionState::Failed | SessionState::Finished
            ) {
                print_detached_diagnostics(&session);
                for event in &state.events {
                    print_event(event, args.json);
                }
                return Ok(state.exit_code.unwrap_or(3));
            }
        }
        if Instant::now() >= deadline {
            return Err((
                AirecError::new(
                    ErrorCode::FirstFrameTimeout,
                    "detached session did not report its first frame within 30 seconds",
                    serde_json::json!({"session": session}),
                ),
                args.json,
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn status(args: JsonArgs) -> Result<i32, (AirecError, bool)> {
    let active = active_sessions().map_err(|error| (error, args.json))?;
    if active.is_empty() {
        return Err((
            AirecError::new(
                ErrorCode::NoActiveSession,
                "no active recording session",
                serde_json::json!({}),
            ),
            args.json,
        ));
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&active).expect("status JSON serialization")
        );
    } else {
        for snapshot in active {
            println!(
                "{} {:?} {} target(s)",
                snapshot.session,
                snapshot.state,
                snapshot.targets.len()
            );
        }
    }
    Ok(0)
}

fn stop(args: crate::args::StopArgs) -> Result<i32, (AirecError, bool)> {
    let active = active_sessions().map_err(|error| (error, args.json))?;
    let session = if let Some(session) = args.session {
        if !active.iter().any(|state| state.session == session) {
            return Err((
                AirecError::new(
                    ErrorCode::NoActiveSession,
                    format!("session '{session}' is not active"),
                    serde_json::json!({"session": session}),
                ),
                args.json,
            ));
        }
        session
    } else {
        match active.as_slice() {
            [] => {
                return Err((
                    AirecError::new(
                        ErrorCode::NoActiveSession,
                        "no active recording session",
                        serde_json::json!({}),
                    ),
                    args.json,
                ));
            }
            [state] => state.session.clone(),
            _ => {
                return Err((
                    AirecError::new(
                        ErrorCode::SessionAmbiguous,
                        "more than one recording session is active",
                        serde_json::json!({"sessions": active.iter().map(|state| &state.session).collect::<Vec<_>>()}),
                    ),
                    args.json,
                ));
            }
        }
    };
    match ipc::request(&session, &ControlRequest::Stop).map_err(|error| (error, args.json))? {
        ControlResponse::Events { events, exit_code } => {
            for event in &events {
                print_event(event, args.json);
            }
            remove_session_metadata(&session);
            Ok(exit_code)
        }
        ControlResponse::Error { code, message } => Err((
            AirecError::new(code, message, serde_json::json!({"session": session})),
            args.json,
        )),
        ControlResponse::Status { .. } => Err((
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                "invalid stop response",
                serde_json::json!({}),
            ),
            args.json,
        )),
    }
}

fn doctor(args: JsonArgs) -> Result<i32, (AirecError, bool)> {
    let wgc = GraphicsCaptureApi::is_supported().unwrap_or(false);
    let encoder = diagnose_encoders();
    let value = serde_json::json!({
        "wgc_supported": wgc,
        "capture_border_suppression": GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false),
        "encoder": {
            "format": "H.264",
            "container": "FMPEG4",
            "hardware_available": encoder.hardware_available,
            "software_available": encoder.software_available,
            "selected": encoder.selected,
            "hardware_error": encoder.hardware_error,
            "software_error": encoder.software_error,
        },
        "audio": false,
        "network": false,
        "keyboard_hook": false,
    });
    if args.json {
        println!("{value}");
    } else {
        println!("WGC: {}", if wgc { "supported" } else { "unavailable" });
        println!(
            "Encoder: H.264 FMPEG4 (selected: {})",
            encoder.selected.unwrap_or("unavailable")
        );
    }
    if wgc && encoder.selected.is_some() {
        Ok(0)
    } else if !wgc {
        Err((
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                "Windows Graphics Capture is unavailable",
                value,
            ),
            args.json,
        ))
    } else {
        Err((
            AirecError::new(
                ErrorCode::EncoderUnavailable,
                "no H.264 Media Foundation encoder is available",
                value,
            ),
            args.json,
        ))
    }
}

fn convert(args: crate::args::ConvertArgs) -> Result<i32, (AirecError, bool)> {
    let output = args
        .out
        .clone()
        .unwrap_or_else(|| args.input.with_extension("gif"));
    let result = airec_capture::convert_mp4_to_gif(
        &args.input,
        &output,
        airec_capture::ConversionOptions {
            fps: args.fps,
            max_width: Some(args.width),
        },
    )
    .map_err(|error| (error, args.json))?;
    let size_bytes = std::fs::metadata(&result.output)
        .map_err(|error| {
            AirecError::new(
                ErrorCode::OutputIoError,
                format!("cannot inspect converted GIF: {error}"),
                serde_json::json!({"component": "convert", "path": result.output}),
            )
        })
        .map_err(|error| (error, args.json))?
        .len();
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "event": "converted",
                "ts": now_string(),
                "input": args.input,
                "file": result.output,
                "width": result.width,
                "height": result.height,
                "frames": result.frames,
                "duration_ms": result.duration_ms,
                "size_bytes": size_bytes,
            })
        );
    } else {
        println!(
            "Converted {} frames to {} ({}x{}, {} bytes)",
            result.frames,
            result.output.display(),
            result.width,
            result.height,
            size_bytes
        );
    }
    Ok(0)
}

fn session_child(config_path: &Path) -> Result<i32, AirecError> {
    let launch: LaunchConfig = read_json_file(config_path)?;
    let _diagnostics = install_diagnostic_stderr(&launch.session)?;
    let path = state_path(&launch.session)?;
    let stop = Arc::new(Mutex::new(None));
    let initial = StateFile {
        snapshot: SessionSnapshot {
            session: launch.session.clone(),
            state: SessionState::Starting,
            pipe: ipc::pipe_display_name(&launch.session),
            targets: launch
                .targets
                .iter()
                .map(|target| TargetSnapshot {
                    target: target.id.clone(),
                    file: target.output.clone(),
                    elapsed_ms: 0,
                    frames: 0,
                    dropped: 0,
                    minimized: target.minimized,
                    stop_reason: None,
                })
                .collect(),
            updated_at: now_string(),
        },
        events: Vec::new(),
        exit_code: None,
    };
    let shared = Arc::new(SharedState {
        path,
        value: Mutex::new(initial),
        response_sent: AtomicBool::new(false),
    });
    persist_state(&shared)?;

    let server_shared = shared.clone();
    let server_stop = stop.clone();
    let server_session = launch.session.clone();
    let response_shared = shared.clone();
    std::thread::spawn(move || {
        let _ = ipc::serve(
            server_session,
            move |request| match request {
                ControlRequest::Status => {
                    let state = server_shared
                        .value
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    ControlResponse::Status {
                        session: state.snapshot.clone(),
                    }
                }
                ControlRequest::Stop => {
                    *server_stop
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(StopReason::Requested);
                    loop {
                        {
                            let state = server_shared
                                .value
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(exit_code) = state.exit_code {
                                let events = state
                                    .events
                                    .iter()
                                    .filter(|event| {
                                        matches!(
                                            event,
                                            Event::TargetLost { .. }
                                                | Event::Saved { .. }
                                                | Event::Error { .. }
                                        )
                                    })
                                    .cloned()
                                    .collect();
                                return ControlResponse::Events { events, exit_code };
                            }
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            },
            move |response| {
                if matches!(response, ControlResponse::Events { .. }) {
                    response_shared.response_sent.store(true, Ordering::Release);
                }
            },
        );
    });

    let options = RecordingOptions {
        started_at: Instant::now(),
        fps: launch.fps,
        quality: launch.quality,
        cursor: launch.cursor,
        effects: launch.effects,
        effect_styles: launch.effect_styles,
        input: launch.input,
        max_duration: Duration::from_millis(launch.max_duration_ms),
        first_frame_timeout: airec_core::FIRST_FRAME_TIMEOUT,
        failure_policy: launch.failure_policy,
        event_log: launch.event_log.clone(),
        verbose: launch.verbose,
    };
    let code = match run_live(
        launch.session.clone(),
        launch.targets,
        options,
        None,
        stop.clone(),
        true,
        Some(shared.clone()),
    ) {
        Ok(code) => code,
        Err(error) => {
            let event = error_to_event(&launch.session, &error);
            update_state(&shared, &event);
            error.exit_code()
        }
    };
    {
        let mut state = shared
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.exit_code = Some(code);
        state.snapshot.state = if code == 0 {
            SessionState::Finished
        } else {
            SessionState::Failed
        };
        state.snapshot.updated_at = now_string();
    }
    persist_state(&shared)?;
    if stop
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        == Some(&StopReason::Requested)
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shared.response_sent.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(code)
}

fn run_live(
    session: String,
    targets: Vec<ResolvedTarget>,
    options: RecordingOptions,
    duration: Option<Duration>,
    stop: Arc<Mutex<Option<StopReason>>>,
    json: bool,
    shared: Option<Arc<SharedState>>,
) -> Result<i32, AirecError> {
    let backend = WindowsCaptureBackend::new();
    let encoder_factory = WindowsEncoderFactory;
    let need_input = options.effects || options.event_log.is_some();
    let timeline_origin_ms = Arc::new(AtomicU64::new(u64::MAX));
    let mouse_hook = if need_input {
        Some(MouseHook::install_with_options(
            options.started_at,
            options.input,
        )?)
    } else {
        None
    };
    if options.verbose {
        eprintln!(
            "airec: session {session}, {} target(s), {} fps, {:?} quality, cursor={}, effects={}",
            targets.len(),
            options.fps,
            options.quality,
            options.cursor,
            options.effects
        );
    }
    let mut pipelines = Vec::new();
    let mut startup_failures = Vec::new();
    let mut prepared = Vec::new();
    for target in targets {
        match encoder_factory.create(&target, &options) {
            Ok(encoder) => prepared.push((target, encoder)),
            Err(error) => {
                startup_failures.push((target.id.clone(), error));
                if options.failure_policy == FailurePolicy::Abort {
                    break;
                }
            }
        }
    }
    if !startup_failures.is_empty() && options.failure_policy == FailurePolicy::Abort {
        discard_prepared(prepared, pipelines);
        return Err(startup_abort_error(&startup_failures));
    }
    prepared.reverse();
    while let Some((target, encoder)) = prepared.pop() {
        match create_pipeline(
            &backend,
            &target,
            &options,
            mouse_hook.as_ref(),
            timeline_origin_ms.clone(),
            encoder,
        ) {
            Ok(pipeline) => pipelines.push(pipeline),
            Err(error) => {
                startup_failures.push((target.id.clone(), error));
                if options.failure_policy == FailurePolicy::Abort {
                    break;
                }
            }
        }
    }
    if !startup_failures.is_empty() && options.failure_policy == FailurePolicy::Abort {
        discard_prepared(prepared, pipelines);
        return Err(startup_abort_error(&startup_failures));
    }
    if pipelines.is_empty() {
        return Err(startup_failures
            .into_iter()
            .next()
            .map(|(_, error)| error)
            .unwrap_or_else(|| {
                AirecError::new(
                    ErrorCode::TargetNotFound,
                    "no capture targets",
                    serde_json::json!({}),
                )
            }));
    }
    let run_options = SessionRunOptions {
        session: session.clone(),
        timeline_started_at: options.started_at,
        timeline_origin_ms: timeline_origin_ms.clone(),
        fps: options.fps,
        first_frame_timeout: options.first_frame_timeout,
        duration,
        max_duration: options.max_duration,
        heartbeat_interval: Duration::from_secs(5),
        failure_policy: options.failure_policy,
        stop,
    };
    let output_shared = shared.clone();
    for (target, error) in &startup_failures {
        let event = Event::Error {
            ts: Timestamp::now(),
            session: session.clone(),
            target: Some(target.clone()),
            code: error.code,
            message: error.message.clone(),
            data: error.data.clone(),
            stop_reason: Some(StopReason::Error),
        };
        emit_runtime_event(&output_shared, json, &event);
    }
    let outcome = run_session(pipelines, run_options, |event| {
        emit_runtime_event(&output_shared, json, event);
    });

    if let (Some(path), Some(hook)) = (&options.event_log, mouse_hook.as_ref()) {
        let mut writer = EventLogWriter::create(path)?;
        let mut subscription = hook.subscribe();
        let origin = timeline_origin_ms.load(Ordering::Acquire);
        let video_end_ms = outcome
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Saved { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        for event in subscription.drain_until(origin.saturating_add(video_end_ms)) {
            if let Some(event) = normalize_input_event(event, origin) {
                writer.write(&event)?;
            }
        }
    }
    if let Some(aggregate) = aggregate_startup_failures(&startup_failures, &outcome) {
        let event = error_to_event(&session, &aggregate);
        emit_runtime_event(&output_shared, json, &event);
        return Ok(ErrorCode::PartialFailure.exit_code());
    }
    Ok(outcome.exit_code)
}

fn aggregate_startup_failures(
    startup_failures: &[(String, AirecError)],
    outcome: &SessionOutcome,
) -> Option<AirecError> {
    if startup_failures.is_empty() {
        return None;
    }
    let has_saved = outcome
        .events
        .iter()
        .any(|event| matches!(event, Event::Saved { .. }));
    if !has_saved {
        return None;
    }
    let successes: Vec<_> = outcome
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Saved {
                target,
                file,
                stop_reason,
                ..
            } if stop_reason.is_intended() => {
                Some(serde_json::json!({"target": target, "file": file}))
            }
            _ => None,
        })
        .collect();
    let mut failed: Vec<_> = startup_failures
        .iter()
        .map(|(target, error)| {
            serde_json::json!({"target": target, "code": error.code, "message": error.message})
        })
        .collect();
    if let Some(error) = &outcome.error {
        if let Some(runtime_failures) = error
            .data
            .get("failures")
            .and_then(|value| value.as_array())
        {
            failed.extend(runtime_failures.iter().cloned());
        } else {
            failed.push(serde_json::json!({
                "target": error.data.get("target").cloned().unwrap_or(serde_json::Value::Null),
                "code": error.code,
                "message": error.message,
            }));
        }
    }
    Some(AirecError::new(
        ErrorCode::PartialFailure,
        "one or more targets failed after at least one recording finalized",
        serde_json::json!({"successes": successes, "failures": failed}),
    ))
}

fn create_pipeline(
    backend: &WindowsCaptureBackend,
    target: &ResolvedTarget,
    options: &RecordingOptions,
    mouse_hook: Option<&MouseHook>,
    timeline_origin_ms: Arc<AtomicU64>,
    encoder: Box<dyn airec_core::PipelineEncoder>,
) -> Result<Pipeline, AirecError> {
    let source = match backend.open(target, options) {
        Ok(source) => source,
        Err(error) => {
            drop(encoder);
            let _ = std::fs::remove_file(&target.output);
            return Err(error);
        }
    };
    let processor: Box<dyn FrameProcessor> = Box::new(ClickProcessor {
        target: target.clone(),
        enabled: options.effects,
        input: mouse_hook.map(MouseHook::subscribe),
        timeline_origin_ms,
        compositor: RippleCompositor::new(options.effect_styles),
        last_viewport: None,
    });
    Ok(Pipeline {
        target: target.output.to_string_lossy().into_owned(),
        kind: match target.kind {
            TargetKind::Monitor => "monitor",
            TargetKind::Window => "window",
        }
        .into(),
        title: target.title.clone(),
        output: target.output.clone(),
        source,
        encoder,
        processor,
    })
}

fn startup_abort_error(startup_failures: &[(String, AirecError)]) -> AirecError {
    let failures: Vec<_> = startup_failures
        .iter()
        .map(|(target, error)| {
            serde_json::json!({"target": target, "code": error.code, "message": error.message})
        })
        .collect();
    AirecError::new(
        ErrorCode::AbortedOnFailure,
        "session aborted because a target failed to initialize",
        serde_json::json!({"failures": failures, "stop_reason": "aborted_on_failure"}),
    )
}

fn discard_prepared(
    prepared: Vec<(ResolvedTarget, Box<dyn airec_core::PipelineEncoder>)>,
    pipelines: Vec<Pipeline>,
) {
    for (target, encoder) in prepared {
        drop(encoder);
        let _ = std::fs::remove_file(target.output);
    }
    for pipeline in pipelines {
        let output = pipeline.output.clone();
        drop(pipeline);
        let _ = std::fs::remove_file(output);
    }
}

struct ClickProcessor {
    target: ResolvedTarget,
    enabled: bool,
    input: Option<MouseSubscription>,
    timeline_origin_ms: Arc<AtomicU64>,
    compositor: RippleCompositor,
    last_viewport: Option<(ScreenRect, ScreenRect)>,
}

fn normalize_input_event(event: InputEvent, origin_ms: u64) -> Option<InputEvent> {
    if origin_ms == u64::MAX {
        return None;
    }
    match event {
        InputEvent::Click {
            t_ms,
            button,
            x,
            y,
            double,
        } => Some(InputEvent::Click {
            t_ms: t_ms.checked_sub(origin_ms)?,
            button,
            x,
            y,
            double,
        }),
        InputEvent::DragStart { t_ms, button, x, y } => Some(InputEvent::DragStart {
            t_ms: t_ms.checked_sub(origin_ms)?,
            button,
            x,
            y,
        }),
        InputEvent::DragEnd { t_ms, button, x, y } => Some(InputEvent::DragEnd {
            t_ms: t_ms.checked_sub(origin_ms)?,
            button,
            x,
            y,
        }),
        InputEvent::Move { t_ms, x, y } => Some(InputEvent::Move {
            t_ms: t_ms.checked_sub(origin_ms)?,
            x,
            y,
        }),
    }
}

impl FrameProcessor for ClickProcessor {
    fn process(&mut self, mut frame: CaptureFrame) -> Result<CaptureFrame, AirecError> {
        if self.enabled
            && let Some(input) = &mut self.input
        {
            let origin = self.timeline_origin_ms.load(Ordering::Acquire);
            let events = input
                .drain_until(frame.t_ms.saturating_add(origin))
                .into_iter()
                .filter_map(|event| normalize_input_event(event, origin))
                .collect::<Vec<_>>();
            if let Some(viewport) = target_viewport(&self.target) {
                self.last_viewport = Some(viewport);
            }
            if let Some((clip, mapping)) = self.last_viewport {
                self.compositor
                    .push_events_mapped(events, clip, mapping, &frame);
                self.compositor.render(&mut frame);
            }
        }
        Ok(letterbox(
            &frame,
            even(self.target.width),
            even(self.target.height),
        ))
    }
}

fn target_viewport(target: &ResolvedTarget) -> Option<(ScreenRect, ScreenRect)> {
    match target.kind {
        TargetKind::Monitor => {
            let index = target.id.trim_start_matches("monitor-").parse().ok()?;
            let monitor = Monitor::from_index(index).ok()?;
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..MONITORINFO::default()
            };
            if !unsafe {
                GetMonitorInfoW(
                    windows::Win32::Graphics::Gdi::HMONITOR(monitor.as_raw_hmonitor()),
                    &mut info,
                )
            }
            .as_bool()
            {
                return None;
            }
            let rect = ScreenRect {
                left: info.rcMonitor.left,
                top: info.rcMonitor.top,
                right: info.rcMonitor.right,
                bottom: info.rcMonitor.bottom,
            };
            Some((rect, rect))
        }
        TargetKind::Window => {
            let hwnd = HWND(target.hwnd? as *mut std::ffi::c_void);
            let mut rect = RECT::default();
            unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
            let mut origin = POINT { x: 0, y: 0 };
            if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
                return None;
            }
            let clip = ScreenRect {
                left: origin.x,
                top: origin.y,
                right: origin.x + rect.right,
                bottom: origin.y + rect.bottom,
            };
            let mut bounds = RECT::default();
            if unsafe {
                DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    (&raw mut bounds).cast(),
                    std::mem::size_of::<RECT>() as u32,
                )
            }
            .is_err()
            {
                bounds = RECT {
                    left: clip.left,
                    top: clip.top,
                    right: clip.right,
                    bottom: clip.bottom,
                };
            }
            let mapping = ScreenRect {
                left: bounds.left,
                top: bounds.top,
                right: bounds.right,
                bottom: bounds.bottom,
            };
            Some((clip, mapping))
        }
    }
}

fn prepare(
    targets: &TargetArgs,
    recording: &RecordingArgs,
) -> Result<(Vec<ResolvedTarget>, RecordingOptions), AirecError> {
    let loaded = crate::config::load()?;
    let options = resolve_recording_options(recording, &loaded.value)?;
    if options.verbose
        && let Some(path) = loaded.path
    {
        eprintln!("airec: config {}", path.display());
    }
    let target_specs = target_specs(targets)?;
    let (output, directory) = output_path(recording);
    let backend = WindowsCaptureBackend::new();
    let resolved = backend.resolve(&target_specs, &output, directory)?;
    Ok((resolved, options))
}

fn resolve_recording_options(
    recording: &RecordingArgs,
    config: &crate::config::AppConfig,
) -> Result<RecordingOptions, AirecError> {
    let fps = recording.fps.or(config.defaults.fps).unwrap_or(30);
    if !(1..=60).contains(&fps) {
        return Err(config_value_error(
            "defaults.fps",
            "must be between 1 and 60",
        ));
    }
    let quality = if let Some(value) = recording.quality {
        quality_from_arg(value)
    } else if let Some(value) = config.defaults.quality.as_deref() {
        parse_quality(value)?
    } else {
        Quality::Medium
    };
    let failure_policy = if let Some(value) = recording.on_failure {
        failure_from_arg(value)
    } else if let Some(value) = config.defaults.on_failure.as_deref() {
        parse_failure_policy(value)?
    } else {
        FailurePolicy::Continue
    };
    let cursor = if recording.cursor {
        true
    } else if recording.no_cursor {
        false
    } else {
        config.defaults.cursor.unwrap_or(true)
    };
    let effects = if recording.effects {
        true
    } else if recording.no_effects {
        false
    } else {
        config.defaults.effects.unwrap_or(true)
    };
    let mut effect_styles = EffectStyles::default();
    effect_styles.click.left_color = resolve_color(
        recording.click_color_left.as_deref(),
        config.effects.click_color_left.as_deref(),
        effect_styles.click.left_color,
        "effects.click_color_left",
    )?;
    effect_styles.click.right_color = resolve_color(
        recording.click_color_right.as_deref(),
        config.effects.click_color_right.as_deref(),
        effect_styles.click.right_color,
        "effects.click_color_right",
    )?;
    effect_styles.click.size = resolve_positive_f32(
        recording.click_size,
        config.effects.click_size,
        effect_styles.click.size,
        "effects.click_size",
    )?;
    effect_styles.click.duration_ms = resolve_positive_u64(
        recording.click_duration_ms,
        config.effects.click_duration_ms,
        effect_styles.click.duration_ms,
        "effects.click_duration_ms",
    )?;
    effect_styles.drag.color = resolve_color(
        recording.drag_color.as_deref(),
        config.effects.drag_color.as_deref(),
        effect_styles.drag.color,
        "effects.drag_color",
    )?;
    effect_styles.drag.size = resolve_positive_f32(
        recording.drag_size,
        config.effects.drag_size,
        effect_styles.drag.size,
        "effects.drag_size",
    )?;
    effect_styles.drag.duration_ms = resolve_positive_u64(
        recording.drag_duration_ms,
        config.effects.drag_duration_ms,
        effect_styles.drag.duration_ms,
        "effects.drag_duration_ms",
    )?;
    effect_styles.trail.color = resolve_color(
        recording.trail_color.as_deref(),
        config.effects.trail_color.as_deref(),
        effect_styles.trail.color,
        "effects.trail_color",
    )?;
    effect_styles.trail.size = resolve_positive_f32(
        recording.trail_size,
        config.effects.trail_size,
        effect_styles.trail.size,
        "effects.trail_size",
    )?;
    effect_styles.trail.duration_ms = resolve_positive_u64(
        recording.trail_duration_ms,
        config.effects.trail_duration_ms,
        effect_styles.trail.duration_ms,
        "effects.trail_duration_ms",
    )?;
    let mut input = InputOptions::default();
    input.drag_threshold_pixels = resolve_positive_u32(
        recording.drag_threshold_px,
        config.effects.drag_threshold_px,
        input.drag_threshold_pixels,
        "effects.drag_threshold_px",
    )?;
    input.drag_threshold_ms = resolve_positive_u64(
        recording.drag_threshold_ms,
        config.effects.drag_threshold_ms,
        input.drag_threshold_ms,
        "effects.drag_threshold_ms",
    )?;
    let max_duration = if let Some(value) = recording.max_duration.as_deref() {
        parse_duration(value)?
    } else if let Some(value) = config.defaults.max_duration.as_deref() {
        parse_duration(value).map_err(|error| {
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                error.message,
                serde_json::json!({
                    "component": "config",
                    "key": "defaults.max_duration",
                    "value": value,
                }),
            )
        })?
    } else {
        Duration::from_secs(30 * 60)
    };
    Ok(RecordingOptions {
        started_at: Instant::now(),
        fps,
        quality,
        cursor,
        effects,
        effect_styles,
        input,
        max_duration,
        first_frame_timeout: airec_core::FIRST_FRAME_TIMEOUT,
        failure_policy,
        event_log: recording.event_log.clone(),
        verbose: recording.verbose,
    })
}

const fn quality_from_arg(value: QualityArg) -> Quality {
    match value {
        QualityArg::Low => Quality::Low,
        QualityArg::Medium => Quality::Medium,
        QualityArg::High => Quality::High,
    }
}

const fn failure_from_arg(value: FailureArg) -> FailurePolicy {
    match value {
        FailureArg::Continue => FailurePolicy::Continue,
        FailureArg::Abort => FailurePolicy::Abort,
    }
}

fn parse_quality(value: &str) -> Result<Quality, AirecError> {
    match value {
        "low" => Ok(Quality::Low),
        "medium" => Ok(Quality::Medium),
        "high" => Ok(Quality::High),
        _ => Err(config_value_error(
            "defaults.quality",
            "must be low, medium, or high",
        )),
    }
}

fn parse_failure_policy(value: &str) -> Result<FailurePolicy, AirecError> {
    match value {
        "continue" => Ok(FailurePolicy::Continue),
        "abort" => Ok(FailurePolicy::Abort),
        _ => Err(config_value_error(
            "defaults.on_failure",
            "must be continue or abort",
        )),
    }
}

fn resolve_color(
    cli: Option<&str>,
    config: Option<&str>,
    default: EffectColor,
    key: &str,
) -> Result<EffectColor, AirecError> {
    if let Some(value) = cli {
        parse_color(value, key, "cli")
    } else if let Some(value) = config {
        parse_color(value, key, "config")
    } else {
        Ok(default)
    }
}

fn parse_color(value: &str, key: &str, component: &str) -> Result<EffectColor, AirecError> {
    let hex = value
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6)
        .ok_or_else(|| value_error(component, key, "must use #RRGGBB"))?;
    let channel = |range| {
        u8::from_str_radix(&hex[range], 16)
            .map_err(|_| value_error(component, key, "must use #RRGGBB"))
    };
    Ok(EffectColor::rgb(
        channel(0..2)?,
        channel(2..4)?,
        channel(4..6)?,
    ))
}

fn resolve_positive_f32(
    cli: Option<f32>,
    config: Option<f32>,
    default: f32,
    key: &str,
) -> Result<f32, AirecError> {
    let value = cli.or(config).unwrap_or(default);
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(config_value_error(key, "must be greater than zero"))
    }
}

fn resolve_positive_u64(
    cli: Option<u64>,
    config: Option<u64>,
    default: u64,
    key: &str,
) -> Result<u64, AirecError> {
    let value = cli.or(config).unwrap_or(default);
    if value > 0 {
        Ok(value)
    } else {
        Err(config_value_error(key, "must be greater than zero"))
    }
}

fn resolve_positive_u32(
    cli: Option<u32>,
    config: Option<u32>,
    default: u32,
    key: &str,
) -> Result<u32, AirecError> {
    let value = cli.or(config).unwrap_or(default);
    if value > 0 {
        Ok(value)
    } else {
        Err(config_value_error(key, "must be greater than zero"))
    }
}

fn config_value_error(key: &str, message: &str) -> AirecError {
    value_error("config", key, message)
}

fn value_error(component: &str, key: &str, message: &str) -> AirecError {
    AirecError::new(
        ErrorCode::CaptureInitFailed,
        format!("invalid {component} value {key}: {message}"),
        serde_json::json!({"component": component, "key": key}),
    )
}

fn target_specs(args: &TargetArgs) -> Result<Vec<CaptureTarget>, AirecError> {
    let mut targets = Vec::new();
    for monitor in &args.monitor {
        if monitor.eq_ignore_ascii_case("all") {
            targets.push(CaptureTarget::AllMonitors);
        } else {
            let index = monitor.parse::<usize>().map_err(|error| {
                AirecError::new(
                    ErrorCode::TargetNotFound,
                    error.to_string(),
                    serde_json::json!({"monitor": monitor}),
                )
            })?;
            targets.push(CaptureTarget::Monitor(index));
        }
    }
    targets.extend(args.window.iter().cloned().map(CaptureTarget::WindowTitle));
    targets.extend(
        args.window_handle
            .iter()
            .copied()
            .map(CaptureTarget::WindowHandle),
    );
    targets.extend(args.process.iter().cloned().map(CaptureTarget::Process));
    if targets.is_empty() {
        targets.push(CaptureTarget::PrimaryMonitor);
    }
    Ok(targets)
}

fn output_path(recording: &RecordingArgs) -> (PathBuf, bool) {
    if let Some(path) = &recording.out {
        (path.clone(), false)
    } else if let Some(path) = &recording.out_dir {
        (path.clone(), true)
    } else {
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        (PathBuf::from(format!("airec-{timestamp}")), true)
    }
}

fn parse_duration(value: &str) -> Result<Duration, AirecError> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let amount = number.parse::<u64>().map_err(|error| {
        AirecError::new(
            ErrorCode::CaptureInitFailed,
            format!("invalid duration '{value}': {error}"),
            serde_json::json!({"duration": value}),
        )
    })?;
    let milliseconds = match unit {
        "ms" => amount,
        "s" => amount.saturating_mul(1_000),
        "m" => amount.saturating_mul(60_000),
        "h" => amount.saturating_mul(3_600_000),
        _ => {
            return Err(AirecError::new(
                ErrorCode::CaptureInitFailed,
                format!("duration '{value}' must end in ms, s, m, or h"),
                serde_json::json!({"duration": value}),
            ));
        }
    };
    Ok(Duration::from_millis(milliseconds))
}

fn update_state(shared: &SharedState, event: &Event) {
    let mut state = shared
        .value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.snapshot.updated_at = now_string();
    match event {
        Event::Started { .. } => state.snapshot.state = SessionState::Recording,
        Event::Heartbeat {
            target,
            elapsed_ms,
            frames,
            dropped,
            minimized,
            ..
        } => {
            if let Some(snapshot) = state
                .snapshot
                .targets
                .iter_mut()
                .find(|snapshot| snapshot.file.to_string_lossy() == target.as_str())
            {
                snapshot.elapsed_ms = *elapsed_ms;
                snapshot.frames = *frames;
                snapshot.dropped = *dropped;
                snapshot.minimized = minimized.unwrap_or(false);
            }
        }
        Event::Saved {
            target,
            stop_reason,
            duration_ms,
            frames,
            ..
        } => {
            if let Some(snapshot) = state
                .snapshot
                .targets
                .iter_mut()
                .find(|snapshot| snapshot.file.to_string_lossy() == target.as_str())
            {
                snapshot.elapsed_ms = *duration_ms;
                snapshot.frames = *frames;
                snapshot.stop_reason = Some(*stop_reason);
            }
        }
        Event::TargetLost { .. } | Event::Error { .. } => {}
    }
    state.events.push(event.clone());
}

fn print_event(event: &Event, json: bool) {
    if json {
        event
            .write_jsonl(std::io::stdout().lock())
            .expect("stdout JSONL write");
    } else {
        match event {
            Event::Started {
                session, targets, ..
            } => println!("● REC {session} {} target(s)", targets.len()),
            Event::Heartbeat {
                target,
                elapsed_ms,
                frames,
                ..
            } => print!(
                "\r● REC {target} {:02}:{:02}  {frames} frames",
                elapsed_ms / 60_000,
                elapsed_ms / 1_000 % 60
            ),
            Event::Saved {
                file, stop_reason, ..
            } => println!("\nSaved {} ({stop_reason:?})", file.display()),
            Event::TargetLost { target, .. } => eprintln!("target lost: {target}"),
            Event::Error { code, message, .. } => eprintln!("{code:?}: {message}"),
        }
    }
}

pub fn print_error(error: &AirecError, json: bool) {
    if json {
        let event = error_to_event("none", error);
        event
            .write_jsonl(std::io::stdout().lock())
            .expect("stdout JSONL write");
    } else {
        let code = serde_json::to_string(&error.code).unwrap_or_else(|_| "ERROR".into());
        eprintln!("{}: {}", code.trim_matches('"'), error.message);
    }
}

fn error_to_event(session: &str, error: &AirecError) -> Event {
    let stop_reason = match error.code {
        ErrorCode::AbortedOnFailure => Some(StopReason::AbortedOnFailure),
        ErrorCode::CaptureInitFailed
        | ErrorCode::EncoderUnavailable
        | ErrorCode::FirstFrameTimeout
        | ErrorCode::OutputIoError
        | ErrorCode::PartialFailure => Some(StopReason::Error),
        _ => None,
    };
    Event::Error {
        ts: Timestamp::now(),
        session: session.into(),
        target: None,
        code: error.code,
        message: error.message.clone(),
        data: error.data.clone(),
        stop_reason,
    }
}

fn emit_runtime_event(shared: &Option<Arc<SharedState>>, json: bool, event: &Event) {
    if let Some(shared) = shared {
        update_state(shared, event);
        let _ = persist_state(shared);
    } else {
        print_event(event, json);
    }
}

fn session_directory() -> Result<PathBuf, AirecError> {
    let directory = std::env::temp_dir().join("airec").join("sessions");
    std::fs::create_dir_all(&directory).map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": directory}),
        )
    })?;
    Ok(directory)
}

fn state_path(session: &str) -> Result<PathBuf, AirecError> {
    Ok(session_directory()?.join(format!("{session}.json")))
}

fn diagnostics_path(session: &str) -> Result<PathBuf, AirecError> {
    Ok(session_directory()?.join(format!("diagnostics-{session}.log")))
}

fn session_states() -> Result<Vec<StateFile>, AirecError> {
    let directory = session_directory()?;
    let entries = std::fs::read_dir(&directory).map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": directory}),
        )
    })?;
    Ok(entries
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with("launch-"))
        .filter_map(|entry| read_json_file::<StateFile>(&entry.path()).ok())
        .collect())
}

fn active_sessions() -> Result<Vec<SessionSnapshot>, AirecError> {
    let states = session_states()?;
    let mut active = Vec::new();
    for state in states {
        let session_id = state.snapshot.session.clone();
        let is_active = matches!(
            state.snapshot.state,
            SessionState::Starting | SessionState::Recording | SessionState::Stopping
        );
        if !is_active {
            remove_session_metadata(&session_id);
        } else if let Ok(ControlResponse::Status { session }) =
            ipc::request(&session_id, &ControlRequest::Status)
        {
            active.push(session);
        }
    }
    Ok(active)
}

fn remove_session_metadata(session: &str) {
    if let Ok(directory) = session_directory() {
        for path in [
            directory.join(format!("{session}.json")),
            directory.join(format!("launch-{session}.json")),
            directory.join(format!("diagnostics-{session}.log")),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn persist_state(shared: &SharedState) -> Result<(), AirecError> {
    let state = shared
        .value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    write_json_file(&shared.path, &*state)
}

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<(), AirecError> {
    let file = File::create(path).map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": path}),
        )
    })?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": path}),
        )
    })?;
    writer.flush().map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": path}),
        )
    })
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, AirecError> {
    let file = File::open(path).map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": path}),
        )
    })?;
    serde_json::from_reader(file).map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": path}),
        )
    })
}

fn spawn_detached(config: &Path) -> Result<(), AirecError> {
    let executable = std::env::current_exe().map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"component": "current_exe"}),
        )
    })?;
    spawn_detached_process(
        &executable,
        &[
            OsStr::new("_session"),
            OsStr::new("--config"),
            config.as_os_str(),
        ],
    )
}

fn spawn_detached_process(executable: &Path, arguments: &[&OsStr]) -> Result<(), AirecError> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW, DETACHED_PROCESS,
        PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows::core::{PCWSTR, PWSTR};

    let mut executable_wide: Vec<u16> = executable.as_os_str().encode_wide().collect();
    executable_wide.push(0);
    let mut command_line = Vec::new();
    push_windows_argument(&mut command_line, executable.as_os_str());
    for argument in arguments {
        command_line.push(u16::from(b' '));
        push_windows_argument(&mut command_line, argument);
    }
    command_line.push(0);

    let startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>())
            .expect("STARTUPINFOW size fits in u32"),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )
        .map_err(|error| {
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                error.to_string(),
                serde_json::json!({"component": "detach"}),
            )
        })?;
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

fn push_windows_argument(command_line: &mut Vec<u16>, argument: &OsStr) {
    use std::os::windows::ffi::OsStrExt;

    command_line.push(u16::from(b'"'));
    let mut backslashes = 0_usize;
    for unit in argument.encode_wide() {
        if unit == u16::from(b'\\') {
            backslashes += 1;
        } else if unit == u16::from(b'"') {
            command_line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
            command_line.push(unit);
            backslashes = 0;
        } else {
            command_line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            command_line.push(unit);
            backslashes = 0;
        }
    }
    command_line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    command_line.push(u16::from(b'"'));
}

#[cfg(debug_assertions)]
fn start_detached_pipe_probe(ready: PathBuf, json: bool) -> Result<i32, (AirecError, bool)> {
    let executable = std::env::current_exe().map_err(|error| {
        (
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                error.to_string(),
                serde_json::json!({"component": "current_exe"}),
            ),
            json,
        )
    })?;
    let hold_ms = std::ffi::OsString::from("5000");
    spawn_detached_process(
        &executable,
        &[
            OsStr::new("_test_hold"),
            OsStr::new("--ready"),
            ready.as_os_str(),
            OsStr::new("--hold-ms"),
            hold_ms.as_os_str(),
        ],
    )
    .map_err(|error| (error, json))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready.exists() {
        if Instant::now() >= deadline {
            return Err((
                AirecError::new(
                    ErrorCode::CaptureInitFailed,
                    "detached pipe probe did not start",
                    serde_json::json!({"component": "detach_test"}),
                ),
                json,
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let event = Event::Started {
        ts: Timestamp::now(),
        session: "pipe-probe".into(),
        target: None,
        targets: Vec::new(),
    };
    print_event(&event, json);
    Ok(0)
}

#[cfg(debug_assertions)]
fn test_hold(args: crate::args::TestHoldArgs) -> Result<i32, AirecError> {
    std::fs::write(&args.ready, b"ready").map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            error.to_string(),
            serde_json::json!({"path": args.ready}),
        )
    })?;
    std::thread::sleep(Duration::from_millis(args.hold_ms));
    Ok(0)
}

fn install_diagnostic_stderr(session: &str) -> Result<File, AirecError> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

    let path = diagnostics_path(session)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .open(&path)
        .map_err(|error| {
            AirecError::new(
                ErrorCode::OutputIoError,
                error.to_string(),
                serde_json::json!({"path": path}),
            )
        })?;
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, HANDLE(file.as_raw_handle())).map_err(|error| {
            AirecError::new(
                ErrorCode::OutputIoError,
                error.to_string(),
                serde_json::json!({"path": path}),
            )
        })?;
    }
    Ok(file)
}

fn print_detached_diagnostics(session: &str) {
    if let Ok(path) = diagnostics_path(session)
        && let Ok(contents) = std::fs::read_to_string(path)
        && !contents.is_empty()
    {
        eprint!("{contents}");
    }
}

fn short_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn even(value: u32) -> u32 {
    value.max(2).next_multiple_of(2)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn duration_parser_supports_cli_contract() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("150ms").unwrap(), Duration::from_millis(150));
        assert!(parse_duration("30").is_err());
    }

    #[test]
    fn default_target_is_primary_monitor() {
        assert_eq!(
            target_specs(&TargetArgs::default()).unwrap(),
            [CaptureTarget::PrimaryMonitor]
        );
    }

    #[test]
    fn startup_failure_and_finalized_target_loss_are_partial_failure() {
        let startup = vec![(
            "monitor-2".into(),
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                "startup",
                serde_json::json!({}),
            ),
        )];
        let outcome = SessionOutcome {
            events: vec![Event::Saved {
                ts: Timestamp::now(),
                session: "test".into(),
                target: "lost.mp4".into(),
                file: "lost.mp4".into(),
                stop_reason: StopReason::TargetLost,
                duration_ms: 1,
                frames: 1,
                size_bytes: 1,
            }],
            exit_code: 8,
            error: Some(AirecError::new(
                ErrorCode::TargetLost,
                "runtime",
                serde_json::json!({"target": "lost.mp4"}),
            )),
        };
        let aggregate = aggregate_startup_failures(&startup, &outcome).unwrap();
        assert_eq!(aggregate.code, ErrorCode::PartialFailure);
        assert_eq!(aggregate.data["successes"].as_array().unwrap().len(), 0);
        assert_eq!(aggregate.data["failures"].as_array().unwrap().len(), 2);
        assert_eq!(aggregate.data["failures"][1]["code"], "TARGET_LOST");
    }

    #[test]
    fn startup_failure_is_not_aggregated_when_nothing_was_saved() {
        let startup = vec![(
            "monitor-2".into(),
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                "startup",
                serde_json::json!({}),
            ),
        )];
        let outcome = SessionOutcome {
            events: vec![Event::Error {
                ts: Timestamp::now(),
                session: "test".into(),
                target: Some("lost.mp4".into()),
                code: ErrorCode::CaptureInitFailed,
                message: "runtime".into(),
                data: serde_json::json!({}),
                stop_reason: Some(StopReason::Error),
            }],
            exit_code: 3,
            error: Some(AirecError::new(
                ErrorCode::CaptureInitFailed,
                "runtime",
                serde_json::json!({"target": "lost.mp4"}),
            )),
        };
        assert!(aggregate_startup_failures(&startup, &outcome).is_none());
    }

    #[test]
    fn partial_failure_merges_startup_and_runtime_failures() {
        let startup = vec![(
            "monitor-2".into(),
            AirecError::new(
                ErrorCode::CaptureInitFailed,
                "startup",
                serde_json::json!({}),
            ),
        )];
        let outcome = SessionOutcome {
            events: vec![Event::Saved {
                ts: Timestamp::now(),
                session: "test".into(),
                target: "good.mp4".into(),
                file: "good.mp4".into(),
                stop_reason: StopReason::DurationLimit,
                duration_ms: 1,
                frames: 1,
                size_bytes: 1,
            }],
            exit_code: 6,
            error: Some(AirecError::new(
                ErrorCode::PartialFailure,
                "runtime",
                serde_json::json!({"failures": [{"target": "monitor-3", "code": "CAPTURE_INIT_FAILED"}]}),
            )),
        };
        let aggregate = aggregate_startup_failures(&startup, &outcome).unwrap();
        assert_eq!(aggregate.code, ErrorCode::PartialFailure);
        assert_eq!(aggregate.data["successes"].as_array().unwrap().len(), 1);
        assert_eq!(aggregate.data["failures"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn input_events_are_normalized_to_the_first_video_frame() {
        let event = InputEvent::Click {
            t_ms: 350,
            button: airec_core::MouseButton::Left,
            x: 10,
            y: 20,
            double: false,
        };
        assert!(matches!(
            normalize_input_event(event, 250),
            Some(InputEvent::Click { t_ms: 100, .. })
        ));
        assert!(
            normalize_input_event(
                InputEvent::Move {
                    t_ms: 249,
                    x: 0,
                    y: 0
                },
                250
            )
            .is_none()
        );
    }

    fn parsed_recording(arguments: &[&str]) -> RecordingArgs {
        let cli = crate::args::Cli::try_parse_from(arguments).unwrap();
        match cli.command {
            Command::Record(args) => args.recording,
            Command::Start(args) => args.recording,
            _ => panic!("expected a recording command"),
        }
    }

    #[test]
    fn config_values_apply_when_cli_values_are_absent() {
        let recording = parsed_recording(&["airec", "record"]);
        let config: crate::config::AppConfig = toml::from_str(
            r##"
                [defaults]
                fps = 24
                quality = "high"
                cursor = false
                effects = false
                max_duration = "2m"
                on_failure = "abort"

                [effects]
                click_color_left = "#112233"
                trail_size = 8.5
                drag_threshold_px = 9
                drag_threshold_ms = 275
            "##,
        )
        .unwrap();
        let options = resolve_recording_options(&recording, &config).unwrap();
        assert_eq!(options.fps, 24);
        assert_eq!(options.quality, Quality::High);
        assert!(!options.cursor);
        assert!(!options.effects);
        assert_eq!(options.max_duration, Duration::from_secs(120));
        assert_eq!(options.failure_policy, FailurePolicy::Abort);
        assert_eq!(
            options.effect_styles.click.left_color,
            EffectColor::rgb(0x11, 0x22, 0x33)
        );
        assert_eq!(options.effect_styles.trail.size, 8.5);
        assert_eq!(options.input.drag_threshold_pixels, 9);
        assert_eq!(options.input.drag_threshold_ms, 275);
    }

    #[test]
    fn explicit_cli_values_override_config_including_boolean_enables() {
        let recording = parsed_recording(&[
            "airec",
            "start",
            "--fps",
            "60",
            "--quality",
            "low",
            "--cursor",
            "--effects",
            "--trail-size",
            "2.0",
            "--on-failure",
            "continue",
        ]);
        let config: crate::config::AppConfig = toml::from_str(
            r#"
                [defaults]
                fps = 12
                quality = "high"
                cursor = false
                effects = false
                on_failure = "abort"

                [effects]
                trail_size = 9.0
            "#,
        )
        .unwrap();
        let options = resolve_recording_options(&recording, &config).unwrap();
        assert_eq!(options.fps, 60);
        assert_eq!(options.quality, Quality::Low);
        assert!(options.cursor);
        assert!(options.effects);
        assert_eq!(options.effect_styles.trail.size, 2.0);
        assert_eq!(options.failure_policy, FailurePolicy::Continue);
    }

    #[test]
    fn malformed_effect_color_is_a_stable_config_error() {
        let recording = parsed_recording(&["airec", "record"]);
        let config: crate::config::AppConfig =
            toml::from_str("[effects]\nclick_color_left = 'yellow'\n").unwrap();
        let error = resolve_recording_options(&recording, &config).unwrap_err();
        assert_eq!(error.code, ErrorCode::CaptureInitFailed);
        assert_eq!(error.data["component"], "config");
        assert_eq!(error.data["key"], "effects.click_color_left");
    }

    #[test]
    fn malformed_config_duration_has_config_error_envelope() {
        let recording = parsed_recording(&["airec", "record"]);
        let config: crate::config::AppConfig =
            toml::from_str("[defaults]\nmax_duration = 'forever'\n").unwrap();
        let error = resolve_recording_options(&recording, &config).unwrap_err();
        assert_eq!(error.code, ErrorCode::CaptureInitFailed);
        assert_eq!(error.data["component"], "config");
        assert_eq!(error.data["key"], "defaults.max_duration");
        assert_eq!(error.data["value"], "forever");
    }

    #[test]
    fn malformed_cli_color_is_attributed_to_cli() {
        let recording = parsed_recording(&["airec", "record", "--click-color-left", "yellow"]);
        let error = resolve_recording_options(&recording, &crate::config::AppConfig::default())
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::CaptureInitFailed);
        assert_eq!(error.data["component"], "cli");
        assert_eq!(error.data["key"], "effects.click_color_left");
    }
}
