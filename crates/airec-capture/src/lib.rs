//! Capture target enumeration, frame sources, and encoders for each supported platform.

mod convert;
#[cfg(target_os = "linux")]
mod linux_backend;
#[cfg(windows)]
mod windows_backend;
mod yuv;

pub use convert::{ConversionOptions, ConversionResult, convert_mp4_to_gif};
#[cfg(target_os = "linux")]
pub use linux_backend::{
    OpenH264EncoderFactory as PlatformEncoderFactory, X11CaptureBackend as PlatformCaptureBackend,
    diagnose_audio, diagnose_capture, diagnose_encoders, open_audio, target_viewport,
};
#[cfg(windows)]
pub use windows_backend::{
    WindowsCaptureBackend as PlatformCaptureBackend,
    WindowsEncoderFactory as PlatformEncoderFactory, diagnose_audio, diagnose_capture,
    diagnose_encoders, open_audio, target_viewport,
};

use std::path::{Path, PathBuf};

use airec_core::{
    AirecError, CaptureTarget, ErrorCode, MonitorInfo, ResolvedTarget, TargetKind, WindowInfo,
    match_process, match_title, sanitize_file_stem,
};

#[derive(Clone, Debug)]
pub struct EncoderDiagnostics {
    pub hardware_available: bool,
    pub software_available: bool,
    pub selected: Option<&'static str>,
    pub hardware_error: Option<String>,
    pub software_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CaptureDiagnostics {
    pub available: bool,
    /// One line for `doctor` text output, such as `WGC: supported`.
    pub summary: String,
    /// Platform-specific `doctor --json` fields.
    pub fields: serde_json::Map<String, serde_json::Value>,
    pub unavailable_message: &'static str,
}

#[derive(Clone, Debug)]
pub struct AudioDiagnostics {
    pub available: bool,
    /// How a window's audio is captured, such as `process_loopback`.
    pub capture: &'static str,
    pub codec: &'static str,
    pub error: Option<String>,
}

fn audio_error(message: impl Into<String>, target: &ResolvedTarget) -> AirecError {
    AirecError::new(
        ErrorCode::CaptureInitFailed,
        message,
        serde_json::json!({"component": "audio", "target": target.id, "pid": target.pid}),
    )
}

/// Screen-space rectangle in physical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenArea {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Where pointer input may land (`clip`) and which screen area the captured frame shows
/// (`mapping`). They differ for a Windows window, whose frame includes the extended frame bounds
/// while clicks are limited to the client area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetViewport {
    pub clip: ScreenArea,
    pub mapping: ScreenArea,
}

fn capture_error(message: impl Into<String>) -> AirecError {
    AirecError::new(ErrorCode::CaptureInitFailed, message, serde_json::json!({}))
}

fn resolve_targets(
    monitors: &[MonitorInfo],
    windows: &[WindowInfo],
    targets: &[CaptureTarget],
    output: &Path,
    output_is_directory: bool,
) -> Result<Vec<ResolvedTarget>, AirecError> {
    let targets = if targets.is_empty() {
        &[CaptureTarget::PrimaryMonitor][..]
    } else {
        targets
    };
    let mut resolved = Vec::new();
    for target in targets {
        match target {
            CaptureTarget::PrimaryMonitor => {
                let monitor = monitors
                    .iter()
                    .find(|monitor| monitor.primary)
                    .ok_or_else(|| {
                        AirecError::new(
                            ErrorCode::TargetNotFound,
                            "primary monitor not found",
                            serde_json::json!({}),
                        )
                    })?;
                resolved.push(resolve_monitor(monitor, output, output_is_directory));
            }
            CaptureTarget::Monitor(index) => {
                let monitor = monitors
                    .iter()
                    .find(|monitor| monitor.index == *index)
                    .ok_or_else(|| {
                        AirecError::new(
                            ErrorCode::TargetNotFound,
                            format!("monitor {index} not found"),
                            serde_json::json!({"index": index, "candidates": monitors}),
                        )
                    })?;
                resolved.push(resolve_monitor(monitor, output, output_is_directory));
            }
            CaptureTarget::AllMonitors => {
                resolved.extend(
                    monitors
                        .iter()
                        .map(|monitor| resolve_monitor(monitor, output, true)),
                );
            }
            CaptureTarget::WindowTitle(query) => {
                resolved.push(resolve_window(
                    match_title(windows, query)?,
                    output,
                    output_is_directory,
                ));
            }
            CaptureTarget::WindowHandle(hwnd) => {
                let window = windows
                    .iter()
                    .find(|window| window.hwnd == *hwnd)
                    .ok_or_else(|| {
                        AirecError::new(
                            ErrorCode::TargetNotFound,
                            format!("window handle {hwnd} not found"),
                            serde_json::json!({"hwnd": hwnd, "candidates": windows}),
                        )
                    })?;
                resolved.push(resolve_window(window, output, output_is_directory));
            }
            CaptureTarget::Process(query) => {
                let matches = match_process(windows, query)?;
                let multiple = matches.len() > 1 || output_is_directory;
                resolved.extend(
                    matches
                        .into_iter()
                        .map(|window| resolve_window(window, output, multiple)),
                );
            }
        }
    }
    if !output_is_directory {
        apply_output_template(&mut resolved, output)?;
    }
    Ok(resolved)
}

fn monitor_index(target: &ResolvedTarget) -> Result<usize, AirecError> {
    target
        .id
        .trim_start_matches("monitor-")
        .parse::<usize>()
        .map_err(|error| capture_error(error.to_string()))
}

fn apply_output_template(
    targets: &mut [ResolvedTarget],
    template: &Path,
) -> Result<(), AirecError> {
    let template = template.to_string_lossy();
    if targets.len() > 1 && !template.contains("{target}") {
        return Err(AirecError::new(
            ErrorCode::OutputIoError,
            "multi-target --out must contain the {target} placeholder or use --out-dir",
            serde_json::json!({"targets": targets.len(), "template": template}),
        ));
    }
    if template.contains("{target}") {
        for target in targets {
            target.output = PathBuf::from(template.replace("{target}", &target.id));
        }
    }
    Ok(())
}

fn output_path(base: &Path, directory: bool, name: &str) -> PathBuf {
    if directory {
        base.join(name)
    } else {
        base.to_path_buf()
    }
}

fn resolve_monitor(monitor: &MonitorInfo, output: &Path, directory: bool) -> ResolvedTarget {
    let id = format!("monitor-{}", monitor.index);
    ResolvedTarget {
        kind: TargetKind::Monitor,
        id: id.clone(),
        title: Some(monitor.name.clone()),
        process: None,
        hwnd: None,
        width: monitor.width,
        height: monitor.height,
        minimized: false,
        output: output_path(output, directory, &format!("{id}.mp4")),
        pid: None,
    }
}

fn resolve_window(window: &WindowInfo, output: &Path, directory: bool) -> ResolvedTarget {
    let stem = sanitize_file_stem(&window.title);
    let name = if stem.is_empty() { "window" } else { &stem };
    let id = format!("{name}-{:x}", window.hwnd);
    ResolvedTarget {
        kind: TargetKind::Window,
        id: id.clone(),
        title: Some(window.title.clone()),
        process: Some(window.process.clone()),
        hwnd: Some(window.hwnd),
        width: window.width,
        height: window.height,
        minimized: window.minimized,
        output: output_path(output, directory, &format!("{id}.mp4")),
        pid: (window.pid != 0).then_some(window.pid),
    }
}

fn output_error(path: &Path, error: std::io::Error) -> AirecError {
    AirecError::new(
        ErrorCode::OutputIoError,
        error.to_string(),
        serde_json::json!({"path": path}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_output_names_are_one_based_and_stable() {
        let monitor = MonitorInfo {
            index: 2,
            name: "Display".into(),
            width: 1920,
            height: 1080,
            primary: false,
            scale: 1.0,
        };
        let target = resolve_monitor(&monitor, Path::new("rec"), true);
        assert_eq!(target.id, "monitor-2");
        assert_eq!(target.output, Path::new("rec").join("monitor-2.mp4"));
    }

    #[test]
    fn window_output_name_contains_sanitized_title_and_hwnd() {
        let window = WindowInfo {
            hwnd: 0x1a2b,
            title: "My App: Settings".into(),
            process: "app.exe".into(),
            pid: 4,
            width: 800,
            height: 600,
            minimized: false,
        };
        let target = resolve_window(&window, Path::new("rec"), true);
        assert_eq!(
            target.output,
            Path::new("rec").join("my-app-settings-1a2b.mp4")
        );
    }

    #[test]
    fn multi_target_out_expands_target_placeholder() {
        let monitor = |index| MonitorInfo {
            index,
            name: format!("Display {index}"),
            width: 1920,
            height: 1080,
            primary: index == 1,
            scale: 1.0,
        };
        let mut targets = vec![
            resolve_monitor(&monitor(1), Path::new("unused"), false),
            resolve_monitor(&monitor(2), Path::new("unused"), false),
        ];
        apply_output_template(&mut targets, Path::new("evidence-{target}.mp4")).unwrap();
        assert_eq!(targets[0].output, Path::new("evidence-monitor-1.mp4"));
        assert_eq!(targets[1].output, Path::new("evidence-monitor-2.mp4"));
    }

    #[test]
    fn multi_target_out_without_placeholder_is_rejected() {
        let monitor = MonitorInfo {
            index: 1,
            name: "Display".into(),
            width: 1920,
            height: 1080,
            primary: true,
            scale: 1.0,
        };
        let mut targets = vec![
            resolve_monitor(&monitor, Path::new("one.mp4"), false),
            resolve_monitor(&monitor, Path::new("one.mp4"), false),
        ];
        let error = apply_output_template(&mut targets, Path::new("one.mp4")).unwrap_err();
        assert_eq!(error.code, ErrorCode::OutputIoError);
    }

    #[test]
    fn medium_quality_meets_prd_1080p_size_budget() {
        let bitrate = airec_core::Quality::Medium.bitrate(1920, 1080, 30);
        let bytes_per_minute = u64::from(bitrate) * 60 / 8;
        assert!(bytes_per_minute <= 20 * 1024 * 1024);
    }
}
