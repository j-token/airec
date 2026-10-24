//! X11 capture through core `GetImage`, OpenH264 encoding, and fragmented MP4 output.
//!
//! X11 is used rather than the xdg-desktop-portal ScreenCast API because the portal requires a
//! person to pick the capture target in a dialog, which an unattended agent cannot do. On a
//! Wayland session only XWayland windows are visible through X11.

pub(crate) mod decoder;
mod encoder;
mod mp4;

pub use encoder::{OpenH264EncoderFactory, diagnose_encoders};

use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use airec_core::{
    AirecError, CaptureBackend, CaptureFrame, CaptureTarget, FrameSource, MonitorInfo,
    RecordingOptions, ResolvedTarget, TargetCatalog, TargetKind, WindowInfo,
};
use crossbeam_channel::{Receiver, Sender};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, MapState, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::{
    CaptureDiagnostics, ScreenArea, TargetViewport, capture_error, monitor_index, resolve_targets,
};

pub struct X11CaptureBackend;

impl X11CaptureBackend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for X11CaptureBackend {
    fn default() -> Self {
        Self::new()
    }
}

struct Display {
    connection: RustConnection,
    root: Window,
    root_width: u16,
    root_height: u16,
}

#[derive(Clone, Copy)]
struct MonitorArea {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

impl Display {
    fn connect() -> Result<Self, AirecError> {
        let (connection, screen) = RustConnection::connect(None).map_err(|error| {
            capture_error(format!(
                "cannot connect to the X11 display ({error}); set DISPLAY to a running X server"
            ))
        })?;
        let screen = &connection.setup().roots[screen];
        let (root, root_width, root_height) =
            (screen.root, screen.width_in_pixels, screen.height_in_pixels);
        Ok(Self {
            connection,
            root,
            root_width,
            root_height,
        })
    }

    fn atom(&self, name: &str) -> Result<Atom, AirecError> {
        Ok(self
            .connection
            .intern_atom(false, name.as_bytes())
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?
            .atom)
    }

    /// Monitors in RandR order, numbered from 1. Servers without RandR 1.5, such as some Xvfb
    /// builds, report the whole root window as a single primary monitor.
    fn monitors(&self) -> Result<Vec<(MonitorInfo, MonitorArea)>, AirecError> {
        let randr_monitors = self
            .connection
            .randr_query_version(1, 5)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|_| self.connection.randr_get_monitors(self.root, true).ok())
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.monitors)
            .unwrap_or_default();
        if randr_monitors.is_empty() {
            let area = MonitorArea {
                x: 0,
                y: 0,
                width: self.root_width,
                height: self.root_height,
            };
            return Ok(vec![(monitor_info(1, "Screen".into(), area, true), area)]);
        }
        let has_primary = randr_monitors.iter().any(|monitor| monitor.primary);
        Ok(randr_monitors
            .iter()
            .enumerate()
            .map(|(offset, monitor)| {
                let index = offset + 1;
                let name = self
                    .connection
                    .get_atom_name(monitor.name)
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
                    .unwrap_or_else(|| format!("Monitor {index}"));
                let area = MonitorArea {
                    x: monitor.x,
                    y: monitor.y,
                    width: monitor.width,
                    height: monitor.height,
                };
                let primary = monitor.primary || (!has_primary && offset == 0);
                (monitor_info(index, name, area, primary), area)
            })
            .collect())
    }

    fn monitor_area(&self, index: usize) -> Result<MonitorArea, AirecError> {
        self.monitors()?
            .into_iter()
            .find(|(monitor, _)| monitor.index == index)
            .map(|(_, area)| area)
            .ok_or_else(|| capture_error(format!("monitor {index} is no longer connected")))
    }

    /// Top-level application windows: the window manager's client list when there is one,
    /// otherwise the mapped children of the root window.
    fn top_level_windows(&self) -> Result<Vec<Window>, AirecError> {
        let client_list = self.atom("_NET_CLIENT_LIST")?;
        let managed = self
            .connection
            .get_property(false, self.root, client_list, AtomEnum::WINDOW, 0, u32::MAX)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?;
        if let Some(windows) = managed.value32() {
            let windows: Vec<_> = windows.collect();
            if !windows.is_empty() {
                return Ok(windows);
            }
        }
        let children = self
            .connection
            .query_tree(self.root)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?
            .children;
        Ok(children
            .into_iter()
            .filter(|&window| {
                self.connection
                    .get_window_attributes(window)
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE)
            })
            .collect())
    }

    fn window_info(&self, window: Window, atoms: &WindowAtoms) -> Option<WindowInfo> {
        let geometry = self.connection.get_geometry(window).ok()?.reply().ok()?;
        if geometry.width == 0 || geometry.height == 0 {
            return None;
        }
        let title = self.window_title(window, atoms)?;
        if title.trim().is_empty() {
            return None;
        }
        let pid = self
            .connection
            .get_property(false, window, atoms.pid, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().and_then(|mut values| values.next()))
            .unwrap_or(0);
        let attributes = self
            .connection
            .get_window_attributes(window)
            .ok()?
            .reply()
            .ok()?;
        Some(WindowInfo {
            hwnd: window as isize,
            title,
            process: process_name(pid).unwrap_or_else(|| "<unavailable>".into()),
            pid,
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
            minimized: attributes.map_state != MapState::VIEWABLE,
        })
    }

    fn window_title(&self, window: Window, atoms: &WindowAtoms) -> Option<String> {
        let property = |name: Atom, kind: Atom| {
            self.connection
                .get_property(false, window, name, kind, 0, 4096)
                .ok()?
                .reply()
                .ok()
                .filter(|reply| !reply.value.is_empty())
                .map(|reply| String::from_utf8_lossy(&reply.value).into_owned())
        };
        property(atoms.net_wm_name, atoms.utf8_string)
            .or_else(|| property(AtomEnum::WM_NAME.into(), AtomEnum::ANY.into()))
    }

    fn window_area(&self, window: Window) -> Result<ScreenArea, ReplyError> {
        let geometry = self.connection.get_geometry(window)?.reply()?;
        let origin = self
            .connection
            .translate_coordinates(window, self.root, 0, 0)?
            .reply()?;
        let left = i32::from(origin.dst_x);
        let top = i32::from(origin.dst_y);
        Ok(ScreenArea {
            left,
            top,
            right: left + i32::from(geometry.width),
            bottom: top + i32::from(geometry.height),
        })
    }
}

struct WindowAtoms {
    net_wm_name: Atom,
    utf8_string: Atom,
    pid: Atom,
}

fn monitor_info(index: usize, name: String, area: MonitorArea, primary: bool) -> MonitorInfo {
    MonitorInfo {
        index,
        name,
        width: u32::from(area.width),
        height: u32::from(area.height),
        primary,
        scale: 1.0,
    }
}

fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(name.trim_end().to_owned())
}

fn x11_error(error: impl std::fmt::Display) -> AirecError {
    capture_error(format!("X11 request failed: {error}"))
}

impl TargetCatalog for X11CaptureBackend {
    fn monitors(&self) -> Result<Vec<MonitorInfo>, AirecError> {
        Ok(Display::connect()?
            .monitors()?
            .into_iter()
            .map(|(monitor, _)| monitor)
            .collect())
    }

    fn windows(&self) -> Result<Vec<WindowInfo>, AirecError> {
        let display = Display::connect()?;
        let atoms = WindowAtoms {
            net_wm_name: display.atom("_NET_WM_NAME")?,
            utf8_string: display.atom("UTF8_STRING")?,
            pid: display.atom("_NET_WM_PID")?,
        };
        Ok(display
            .top_level_windows()?
            .into_iter()
            .filter_map(|window| display.window_info(window, &atoms))
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
pub fn diagnose_capture() -> CaptureDiagnostics {
    let display = std::env::var("DISPLAY").ok();
    let connection = Display::connect();
    let mut fields = serde_json::Map::new();
    fields.insert("display".into(), display.into());
    fields.insert(
        "wayland_session".into(),
        std::env::var_os("WAYLAND_DISPLAY").is_some().into(),
    );
    let available = match &connection {
        Ok(display) => {
            fields.insert("x11_supported".into(), true.into());
            fields.insert(
                "xfixes_cursor".into(),
                display
                    .connection
                    .xfixes_query_version(4, 0)
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .is_some()
                    .into(),
            );
            fields.insert(
                "monitors".into(),
                display
                    .monitors()
                    .map_or(0, |monitors| monitors.len())
                    .into(),
            );
            true
        }
        Err(error) => {
            fields.insert("x11_supported".into(), false.into());
            fields.insert("x11_error".into(), error.message.clone().into());
            false
        }
    };
    CaptureDiagnostics {
        available,
        summary: format!(
            "X11: {}",
            if available {
                "supported"
            } else {
                "unavailable"
            }
        ),
        fields,
        unavailable_message: "no X11 display is reachable",
    }
}

thread_local! {
    // The effects compositor asks for the viewport on every frame; reconnecting each time
    // would cost far more than the query.
    static VIEWPORT_DISPLAY: RefCell<Option<Display>> = const { RefCell::new(None) };
}

#[must_use]
pub fn target_viewport(target: &ResolvedTarget) -> Option<TargetViewport> {
    VIEWPORT_DISPLAY.with_borrow_mut(|cached| {
        if cached.is_none() {
            *cached = Display::connect().ok();
        }
        let display = cached.as_ref()?;
        let area = match target.kind {
            TargetKind::Monitor => {
                let monitor = display.monitor_area(monitor_index(target).ok()?).ok()?;
                let left = i32::from(monitor.x);
                let top = i32::from(monitor.y);
                ScreenArea {
                    left,
                    top,
                    right: left + i32::from(monitor.width),
                    bottom: top + i32::from(monitor.height),
                }
            }
            TargetKind::Window => display.window_area(target.hwnd? as Window).ok()?,
        };
        Some(TargetViewport {
            clip: area,
            mapping: area,
        })
    })
}

enum CaptureMessage {
    Frame(CaptureFrame),
    Closed,
}

enum CaptureRegion {
    Monitor(MonitorArea),
    Window(Window),
}

struct CaptureLoop {
    display: Display,
    region: CaptureRegion,
    cursor: bool,
    interval: Duration,
    started_at: Instant,
    sender: Sender<CaptureMessage>,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    minimized: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

enum Grab {
    Frame(CaptureFrame),
    Unavailable,
    TargetGone,
}

impl CaptureLoop {
    fn run(self) {
        let mut next_tick = Instant::now();
        while !self.stop.load(Ordering::Acquire) {
            match self.grab() {
                Ok(Grab::Frame(frame)) => {
                    if let Err(crossbeam_channel::TrySendError::Full(_)) =
                        self.sender.try_send(CaptureMessage::Frame(frame))
                    {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(Grab::Unavailable) => {}
                Ok(Grab::TargetGone) => {
                    self.closed.store(true, Ordering::Release);
                    let _ = self.sender.try_send(CaptureMessage::Closed);
                    return;
                }
                Err(error) => {
                    eprintln!("warning: X11 capture stopped: {}", error.message);
                    return;
                }
            }
            next_tick += self.interval;
            let now = Instant::now();
            if next_tick > now {
                std::thread::sleep(next_tick - now);
            } else {
                next_tick = now;
            }
        }
    }

    fn grab(&self) -> Result<Grab, AirecError> {
        let connection = &self.display.connection;
        let (drawable, area) = match self.region {
            CaptureRegion::Monitor(monitor) => (
                self.display.root,
                ScreenArea {
                    left: i32::from(monitor.x),
                    top: i32::from(monitor.y),
                    right: i32::from(monitor.x) + i32::from(monitor.width),
                    bottom: i32::from(monitor.y) + i32::from(monitor.height),
                },
            ),
            CaptureRegion::Window(window) => {
                let attributes = match connection
                    .get_window_attributes(window)
                    .map_err(x11_error)?
                    .reply()
                {
                    Ok(attributes) => attributes,
                    Err(error) if is_missing_window(&error) => return Ok(Grab::TargetGone),
                    Err(error) => return Err(x11_error(error)),
                };
                let viewable = attributes.map_state == MapState::VIEWABLE;
                self.minimized.store(!viewable, Ordering::Release);
                if !viewable {
                    return Ok(Grab::Unavailable);
                }
                match self.display.window_area(window) {
                    Ok(area) => (window, area),
                    Err(error) if is_missing_window(&error) => return Ok(Grab::TargetGone),
                    Err(_) => return Ok(Grab::Unavailable),
                }
            }
        };
        let width = u16::try_from(area.right - area.left).unwrap_or(0);
        let height = u16::try_from(area.bottom - area.top).unwrap_or(0);
        if width == 0 || height == 0 {
            return Ok(Grab::Unavailable);
        }
        let (x, y) = match self.region {
            CaptureRegion::Monitor(monitor) => (monitor.x, monitor.y),
            CaptureRegion::Window(_) => (0, 0),
        };
        let t_ms = self.started_at.elapsed().as_millis() as u64;
        let image = match connection
            .get_image(ImageFormat::Z_PIXMAP, drawable, x, y, width, height, !0)
            .map_err(x11_error)?
            .reply()
        {
            Ok(image) => image,
            Err(error) if is_missing_window(&error) => return Ok(Grab::TargetGone),
            // BadMatch: the window was unmapped or moved partly off-screen between requests.
            Err(ReplyError::X11Error(error)) if error.error_kind == ErrorKind::Match => {
                return Ok(Grab::Unavailable);
            }
            Err(error) => return Err(x11_error(error)),
        };
        let mut frame = bgra_frame(
            &self.display.connection,
            image.depth,
            image.data,
            width,
            height,
        )?;
        frame.t_ms = t_ms;
        if self.cursor {
            draw_cursor(connection, &mut frame, area);
        }
        Ok(Grab::Frame(frame))
    }
}

fn is_missing_window(error: &ReplyError) -> bool {
    matches!(
        error,
        ReplyError::X11Error(error)
            if matches!(error.error_kind, ErrorKind::Window | ErrorKind::Drawable)
    )
}

/// Z-pixmap images at depth 24 or 32 are 32 bits per pixel. With least-significant-byte-first
/// order that is BGRX, the layout the encoder expects; the unused byte is set opaque.
fn bgra_frame(
    connection: &RustConnection,
    depth: u8,
    mut data: Vec<u8>,
    width: u16,
    height: u16,
) -> Result<CaptureFrame, AirecError> {
    let setup = connection.setup();
    let bits_per_pixel = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .map(|format| format.bits_per_pixel);
    if bits_per_pixel != Some(32) || setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err(capture_error(format!(
            "unsupported X11 pixel format: depth {depth}, {bits_per_pixel:?} bits per pixel, {:?} byte order",
            setup.image_byte_order
        )));
    }
    let stride = u32::from(width) * 4;
    if data.len() < stride as usize * usize::from(height) {
        return Err(capture_error("X11 returned a truncated image"));
    }
    for pixel in data.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Ok(CaptureFrame {
        width: u32::from(width),
        height: u32::from(height),
        stride,
        bgra: data,
        t_ms: 0,
    })
}

/// Core `GetImage` never contains the pointer, so it is composited from the XFixes cursor
/// image, which is premultiplied ARGB.
fn draw_cursor(connection: &RustConnection, frame: &mut CaptureFrame, area: ScreenArea) {
    let Some(cursor) = connection
        .xfixes_get_cursor_image()
        .ok()
        .and_then(|cookie| cookie.reply().ok())
    else {
        return;
    };
    let left = i32::from(cursor.x) - i32::from(cursor.xhot) - area.left;
    let top = i32::from(cursor.y) - i32::from(cursor.yhot) - area.top;
    let cursor_width = usize::from(cursor.width);
    for (row, pixels) in cursor
        .cursor_image
        .chunks_exact(cursor_width.max(1))
        .enumerate()
    {
        let y = top + row as i32;
        if y < 0 || y >= frame.height as i32 {
            continue;
        }
        for (column, &argb) in pixels.iter().enumerate() {
            let x = left + column as i32;
            let alpha = argb >> 24;
            if x < 0 || x >= frame.width as i32 || alpha == 0 {
                continue;
            }
            let offset = y as usize * frame.stride as usize + x as usize * 4;
            let destination = &mut frame.bgra[offset..offset + 3];
            for (channel, value) in destination.iter_mut().enumerate() {
                let source = (argb >> (channel * 8)) & 0xff;
                *value = (source + u32::from(*value) * (255 - alpha) / 255).min(255) as u8;
            }
        }
    }
}

struct X11FrameSource {
    receiver: Receiver<CaptureMessage>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    closed: Arc<AtomicBool>,
    minimized: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    drop_baseline: Option<u64>,
}

impl Drop for X11FrameSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl FrameSource for X11FrameSource {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CaptureFrame>, AirecError> {
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
                if self.closed.load(Ordering::Acquire) {
                    Ok(None)
                } else {
                    Err(capture_error("capture thread ended unexpectedly"))
                }
            }
        }
    }

    fn is_target_alive(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    fn is_minimized(&self) -> bool {
        self.minimized.load(Ordering::Acquire)
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped
            .load(Ordering::Relaxed)
            .saturating_sub(self.drop_baseline.unwrap_or(0))
    }
}

impl CaptureBackend for X11CaptureBackend {
    fn open(
        &self,
        target: &ResolvedTarget,
        options: &RecordingOptions,
    ) -> Result<Box<dyn FrameSource>, AirecError> {
        let display = Display::connect()?;
        let region = match target.kind {
            TargetKind::Monitor => {
                CaptureRegion::Monitor(display.monitor_area(monitor_index(target)?)?)
            }
            TargetKind::Window => CaptureRegion::Window(
                target
                    .hwnd
                    .ok_or_else(|| capture_error("resolved window is missing its X11 id"))?
                    as Window,
            ),
        };
        let cursor = options.cursor
            && display
                .connection
                .xfixes_query_version(4, 0)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some();
        if options.cursor && !cursor {
            eprintln!(
                "warning: the X server has no XFixes cursor support; recording without the pointer"
            );
        }

        let (sender, receiver) = crossbeam_channel::bounded(3);
        let stop = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let minimized = Arc::new(AtomicBool::new(target.minimized));
        let dropped = Arc::new(AtomicU64::new(0));
        let capture = CaptureLoop {
            display,
            region,
            cursor,
            interval: Duration::from_secs_f64(1.0 / f64::from(options.fps)),
            started_at: options.started_at,
            sender,
            stop: stop.clone(),
            closed: closed.clone(),
            minimized: minimized.clone(),
            dropped: dropped.clone(),
        };
        let thread = std::thread::Builder::new()
            .name(format!("airec-x11-{}", target.id))
            .spawn(move || capture.run())
            .map_err(|error| capture_error(error.to_string()))?;
        Ok(Box::new(X11FrameSource {
            receiver,
            stop,
            thread: Some(thread),
            closed,
            minimized,
            dropped,
            drop_baseline: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_process_id_has_no_name() {
        assert_eq!(process_name(0), None);
    }

    #[test]
    fn own_process_name_is_read_from_proc() {
        let name = process_name(std::process::id()).unwrap();

        assert!(!name.is_empty());
        assert!(!name.ends_with('\n'));
    }
}
