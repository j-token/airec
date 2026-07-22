//! Windows Graphics Capture target enumeration and frame sources.

mod encoder;

pub use encoder::{EncoderDiagnostics, WindowsEncoderFactory, diagnose_encoders};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use airec_core::{
    AirecError, CaptureBackend, CaptureTarget, ErrorCode, FrameSource, MonitorInfo,
    RecordingOptions, ResolvedTarget, TargetCatalog, TargetKind, WindowInfo, match_process,
    match_title, sanitize_file_stem,
};
use crossbeam_channel::{Receiver, Sender};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

pub struct WindowsCaptureBackend;

impl WindowsCaptureBackend {
    #[must_use]
    pub fn new() -> Self {
        // Failure means the host fixed awareness before airec loaded. Capture can still proceed.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self
    }
}

impl Default for WindowsCaptureBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn capture_error(message: impl Into<String>) -> AirecError {
    AirecError::new(ErrorCode::CaptureInitFailed, message, serde_json::json!({}))
}

impl TargetCatalog for WindowsCaptureBackend {
    fn monitors(&self) -> Result<Vec<MonitorInfo>, AirecError> {
        let primary = Monitor::primary().map_err(|error| capture_error(error.to_string()))?;
        Monitor::enumerate()
            .map_err(|error| capture_error(error.to_string()))?
            .into_iter()
            .map(|monitor| {
                let index = monitor
                    .index()
                    .map_err(|error| capture_error(error.to_string()))?;
                let mut dpi_x = 96;
                let mut dpi_y = 96;
                let _ = unsafe {
                    GetDpiForMonitor(
                        windows::Win32::Graphics::Gdi::HMONITOR(monitor.as_raw_hmonitor()),
                        MDT_EFFECTIVE_DPI,
                        &mut dpi_x,
                        &mut dpi_y,
                    )
                };
                Ok(MonitorInfo {
                    index,
                    name: monitor
                        .name()
                        .or_else(|_| monitor.device_name())
                        .unwrap_or_else(|_| format!("Monitor {index}")),
                    width: monitor
                        .width()
                        .map_err(|error| capture_error(error.to_string()))?,
                    height: monitor
                        .height()
                        .map_err(|error| capture_error(error.to_string()))?,
                    primary: monitor == primary,
                    scale: f64::from(dpi_x) / 96.0,
                })
            })
            .collect()
    }

    fn windows(&self) -> Result<Vec<WindowInfo>, AirecError> {
        Window::enumerate()
            .map_err(|error| capture_error(error.to_string()))?
            .into_iter()
            .filter_map(|window| {
                let width = window.width().ok()?;
                let height = window.height().ok()?;
                if width <= 0 || height <= 0 {
                    return None;
                }
                let title = window.title().ok()?;
                if title.trim().is_empty() {
                    return None;
                }
                let hwnd = window.as_raw_hwnd() as isize;
                Some(WindowInfo {
                    hwnd,
                    title,
                    process: window
                        .process_name()
                        .unwrap_or_else(|_| "<unavailable>".into()),
                    pid: window.process_id().ok()?,
                    width: width as u32,
                    height: height as u32,
                    minimized: unsafe {
                        IsIconic(windows::Win32::Foundation::HWND(window.as_raw_hwnd()))
                    }
                    .as_bool(),
                })
            })
            .collect::<Vec<_>>()
            .pipe(Ok)
    }

    fn resolve(
        &self,
        targets: &[CaptureTarget],
        output: &Path,
        output_is_directory: bool,
    ) -> Result<Vec<ResolvedTarget>, AirecError> {
        let targets = if targets.is_empty() {
            &[CaptureTarget::PrimaryMonitor][..]
        } else {
            targets
        };
        let monitors = self.monitors()?;
        let windows = self.windows()?;
        let mut resolved = Vec::new();
        for target in targets {
            match target {
                CaptureTarget::PrimaryMonitor => {
                    let monitor =
                        monitors
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
                        match_title(&windows, query)?,
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
                    let matches = match_process(&windows, query)?;
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

trait Pipe: Sized {
    fn pipe<T>(self, function: impl FnOnce(Self) -> T) -> T {
        function(self)
    }
}
impl<T> Pipe for T {}

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
    }
}

enum CaptureMessage {
    Frame(airec_core::CaptureFrame),
    Closed,
}

#[derive(Clone)]
struct CaptureFlags {
    sender: Sender<CaptureMessage>,
    started_at: std::time::Instant,
    dropped: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
}

struct Handler {
    sender: Sender<CaptureMessage>,
    started_at: std::time::Instant,
    dropped: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
}

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

impl GraphicsCaptureApiHandler for Handler {
    type Flags = CaptureFlags;
    type Error = HandlerError;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            sender: context.flags.sender,
            started_at: context.flags.started_at,
            dropped: context.flags.dropped,
            closed: context.flags.closed,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        let mut buffer = frame.buffer()?;
        let stride = buffer.row_pitch();
        let message = CaptureMessage::Frame(airec_core::CaptureFrame {
            width,
            height,
            stride,
            bgra: buffer.as_raw_buffer().to_vec(),
            t_ms: self.started_at.elapsed().as_millis() as u64,
        });
        if let Err(crossbeam_channel::TrySendError::Full(_)) = self.sender.try_send(message) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.closed.store(true, Ordering::Release);
        let _ = self.sender.try_send(CaptureMessage::Closed);
        Ok(())
    }
}

struct WgcFrameSource {
    receiver: Receiver<CaptureMessage>,
    control: Option<CaptureControl<Handler, HandlerError>>,
    closed: Arc<AtomicBool>,
    hwnd: Option<isize>,
    dropped: Arc<AtomicU64>,
    drop_baseline: Option<u64>,
}

impl Drop for WgcFrameSource {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

impl FrameSource for WgcFrameSource {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<airec_core::CaptureFrame>, AirecError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(CaptureMessage::Frame(frame)) => {
                self.drop_baseline
                    .get_or_insert_with(|| self.dropped.load(Ordering::Relaxed));
                Ok(Some(frame))
            }
            Ok(CaptureMessage::Closed) => {
                self.closed.store(true, Ordering::Release);
                Ok(None)
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                self.closed.store(true, Ordering::Release);
                Err(capture_error("capture thread ended unexpectedly"))
            }
        }
    }

    fn is_target_alive(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self.hwnd.is_none_or(|hwnd| unsafe {
                IsWindow(Some(windows::Win32::Foundation::HWND(
                    hwnd as *mut std::ffi::c_void,
                )))
                .as_bool()
            })
    }

    fn is_minimized(&self) -> bool {
        self.hwnd.is_some_and(|hwnd| unsafe {
            IsIconic(windows::Win32::Foundation::HWND(
                hwnd as *mut std::ffi::c_void,
            ))
            .as_bool()
        })
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped
            .load(Ordering::Relaxed)
            .saturating_sub(self.drop_baseline.unwrap_or(0))
    }
}

impl CaptureBackend for WindowsCaptureBackend {
    fn open(
        &self,
        target: &ResolvedTarget,
        options: &RecordingOptions,
    ) -> Result<Box<dyn FrameSource>, AirecError> {
        let (sender, receiver) = crossbeam_channel::bounded(3);
        let dropped = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let flags = CaptureFlags {
            sender,
            started_at: options.started_at,
            dropped: dropped.clone(),
            closed: closed.clone(),
        };
        let cursor = if options.cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };
        let interval = MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(
            1.0 / f64::from(options.fps),
        ));
        let control = match target.kind {
            TargetKind::Monitor => {
                let index = target
                    .id
                    .trim_start_matches("monitor-")
                    .parse::<usize>()
                    .map_err(|error| capture_error(error.to_string()))?;
                let monitor =
                    Monitor::from_index(index).map_err(|error| capture_error(error.to_string()))?;
                start_with_border_fallback(
                    capture_settings(
                        monitor,
                        cursor,
                        interval,
                        DrawBorderSettings::WithoutBorder,
                        flags.clone(),
                    ),
                    || {
                        capture_settings(
                            monitor,
                            cursor,
                            interval,
                            DrawBorderSettings::Default,
                            flags.clone(),
                        )
                    },
                )?
            }
            TargetKind::Window => {
                let hwnd = target
                    .hwnd
                    .ok_or_else(|| capture_error("resolved window is missing HWND"))?;
                let window = Window::from_raw_hwnd(hwnd as *mut std::ffi::c_void);
                start_with_border_fallback(
                    capture_settings(
                        window,
                        cursor,
                        interval,
                        DrawBorderSettings::WithoutBorder,
                        flags.clone(),
                    ),
                    || {
                        capture_settings(
                            window,
                            cursor,
                            interval,
                            DrawBorderSettings::Default,
                            flags.clone(),
                        )
                    },
                )?
            }
        };

        Ok(Box::new(WgcFrameSource {
            receiver,
            control: Some(control),
            closed,
            hwnd: target.hwnd,
            dropped,
            drop_baseline: None,
        }))
    }
}

fn capture_settings<T>(
    item: T,
    cursor: CursorCaptureSettings,
    interval: MinimumUpdateIntervalSettings,
    border: DrawBorderSettings,
    flags: CaptureFlags,
) -> Settings<CaptureFlags, T>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType>,
{
    Settings::new(
        item,
        cursor,
        border,
        SecondaryWindowSettings::Exclude,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    )
}

fn start_with_border_fallback<T, F>(
    preferred: Settings<CaptureFlags, T>,
    fallback: F,
) -> Result<CaptureControl<Handler, HandlerError>, AirecError>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType> + Send + 'static,
    F: FnOnce() -> Settings<CaptureFlags, T>,
{
    if !GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
        eprintln!(
            "warning: this Windows version cannot disable the WGC capture border; continuing with OS default"
        );
        return Handler::start_free_threaded(fallback())
            .map_err(|error| capture_error(error.to_string()));
    }
    match Handler::start_free_threaded(preferred) {
        Ok(control) => Ok(control),
        Err(error) => {
            eprintln!(
                "warning: WGC border suppression was unavailable ({error}); continuing with OS default"
            );
            Handler::start_free_threaded(fallback())
                .map_err(|fallback_error| capture_error(fallback_error.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn close_signal_is_recorded_when_frame_channel_is_full() {
        let (sender, _receiver) = crossbeam_channel::bounded(1);
        sender
            .try_send(CaptureMessage::Frame(airec_core::CaptureFrame {
                width: 1,
                height: 1,
                stride: 4,
                bgra: vec![0; 4],
                t_ms: 0,
            }))
            .unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let mut handler = Handler {
            sender,
            started_at: Instant::now(),
            dropped: Arc::new(AtomicU64::new(0)),
            closed: closed.clone(),
        };

        handler.on_closed().unwrap();

        assert!(closed.load(Ordering::Acquire));
    }

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
}
