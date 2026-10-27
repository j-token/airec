//! Windows Graphics Capture frame sources and Media Foundation encoding.

mod audio;
pub(crate) mod decoder;
mod encoder;

pub use audio::{diagnose_audio, open_audio};
pub use encoder::{WindowsEncoderFactory, diagnose_encoders};

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use airec_core::{
    AirecError, CaptureBackend, CaptureTarget, FrameSource, MonitorInfo, RecordingOptions,
    ResolvedTarget, TargetCatalog, TargetKind, WindowInfo,
};
use crossbeam_channel::{Receiver, Sender};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{ClientToScreen, GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsIconic, IsWindow};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::{
    CaptureDiagnostics, ScreenArea, TargetViewport, capture_error, monitor_index, resolve_targets,
};

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

#[must_use]
pub fn diagnose_capture() -> CaptureDiagnostics {
    let wgc = GraphicsCaptureApi::is_supported().unwrap_or(false);
    let mut fields = serde_json::Map::new();
    fields.insert("wgc_supported".into(), wgc.into());
    fields.insert(
        "capture_border_suppression".into(),
        GraphicsCaptureApi::is_border_settings_supported()
            .unwrap_or(false)
            .into(),
    );
    CaptureDiagnostics {
        available: wgc,
        summary: format!("WGC: {}", if wgc { "supported" } else { "unavailable" }),
        fields,
        unavailable_message: "Windows Graphics Capture is unavailable",
    }
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
                        HMONITOR(monitor.as_raw_hmonitor()),
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
        Ok(Window::enumerate()
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
                    minimized: unsafe { IsIconic(HWND(window.as_raw_hwnd())) }.as_bool(),
                })
            })
            .collect())
    }

    fn resolve(
        &self,
        targets: &[CaptureTarget],
        output: &Path,
        output_is_directory: bool,
    ) -> Result<Vec<ResolvedTarget>, AirecError> {
        resolve_targets(
            &self.monitors()?,
            &self.windows()?,
            targets,
            output,
            output_is_directory,
        )
    }
}

#[must_use]
pub fn target_viewport(target: &ResolvedTarget) -> Option<TargetViewport> {
    match target.kind {
        TargetKind::Monitor => {
            let monitor = Monitor::from_index(monitor_index(target).ok()?).ok()?;
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..MONITORINFO::default()
            };
            if !unsafe { GetMonitorInfoW(HMONITOR(monitor.as_raw_hmonitor()), &mut info) }.as_bool()
            {
                return None;
            }
            let area = screen_area(info.rcMonitor);
            Some(TargetViewport {
                clip: area,
                mapping: area,
            })
        }
        TargetKind::Window => {
            let hwnd = HWND(target.hwnd? as *mut std::ffi::c_void);
            let mut rect = RECT::default();
            unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
            let mut origin = POINT { x: 0, y: 0 };
            if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
                return None;
            }
            let clip = ScreenArea {
                left: origin.x,
                top: origin.y,
                right: origin.x + rect.right,
                bottom: origin.y + rect.bottom,
            };
            let mut bounds = RECT::default();
            let mapping = if unsafe {
                DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    (&raw mut bounds).cast(),
                    std::mem::size_of::<RECT>() as u32,
                )
            }
            .is_ok()
            {
                screen_area(bounds)
            } else {
                clip
            };
            Some(TargetViewport { clip, mapping })
        }
    }
}

fn screen_area(rect: RECT) -> ScreenArea {
    ScreenArea {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
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
                IsWindow(Some(HWND(hwnd as *mut std::ffi::c_void))).as_bool()
            })
    }

    fn is_minimized(&self) -> bool {
        self.hwnd
            .is_some_and(|hwnd| unsafe { IsIconic(HWND(hwnd as *mut std::ffi::c_void)).as_bool() })
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
                let monitor = Monitor::from_index(monitor_index(target)?)
                    .map_err(|error| capture_error(error.to_string()))?;
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
}
