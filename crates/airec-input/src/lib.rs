//! Mouse-only low-level input collection. This crate intentionally has no keyboard APIs.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use airec_core::{AirecError, ErrorCode, InputEvent, InputSource, MouseButton};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, MSLLHOOKSTRUCT, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

#[derive(Default)]
struct HookState {
    timeline: Option<std::sync::Arc<Mutex<Vec<InputEvent>>>>,
    started_at: Option<Instant>,
    last_move_ms: u64,
    last_click: Option<(MouseButton, u64, i32, i32)>,
    pressed: Option<(MouseButton, i32, i32)>,
    drag_active: bool,
}

static HOOK_STATE: OnceLock<Mutex<HookState>> = OnceLock::new();

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if let Ok(mut state) = HOOK_STATE
            .get_or_init(|| Mutex::new(HookState::default()))
            .lock()
            && let (Some(timeline), Some(started_at)) = (&state.timeline, state.started_at)
        {
            let timeline = timeline.clone();
            let t_ms = started_at.elapsed().as_millis() as u64;
            let (x, y) = (data.pt.x, data.pt.y);
            let message = wparam.0 as u32;
            match message {
                WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                    let button = if message == WM_LBUTTONDOWN {
                        MouseButton::Left
                    } else {
                        MouseButton::Right
                    };
                    state.pressed = Some((button, x, y));
                    state.drag_active = false;
                }
                WM_LBUTTONUP | WM_RBUTTONUP => {
                    let button = if message == WM_LBUTTONUP {
                        MouseButton::Left
                    } else {
                        MouseButton::Right
                    };
                    let was_drag = state.drag_active;
                    if was_drag {
                        push_event(&timeline, InputEvent::DragEnd { t_ms, button, x, y });
                    }
                    let threshold = u64::from(unsafe { GetDoubleClickTime() });
                    let double =
                        state
                            .last_click
                            .is_some_and(|(last_button, last_ms, last_x, last_y)| {
                                last_button == button
                                    && t_ms.saturating_sub(last_ms) <= threshold
                                    && (x - last_x).abs() <= 4
                                    && (y - last_y).abs() <= 4
                            });
                    state.last_click = Some((button, t_ms, x, y));
                    state.pressed = None;
                    state.drag_active = false;
                    if !was_drag {
                        push_event(
                            &timeline,
                            InputEvent::Click {
                                t_ms,
                                button,
                                x,
                                y,
                                double,
                            },
                        );
                    }
                }
                WM_MOUSEMOVE if t_ms.saturating_sub(state.last_move_ms) >= 100 => {
                    state.last_move_ms = t_ms;
                    if let Some((button, start_x, start_y)) = state.pressed
                        && !state.drag_active
                        && ((x - start_x).abs() > 4 || (y - start_y).abs() > 4)
                    {
                        state.drag_active = true;
                        push_event(
                            &timeline,
                            InputEvent::DragStart {
                                t_ms,
                                button,
                                x: start_x,
                                y: start_y,
                            },
                        );
                    }
                    push_event(&timeline, InputEvent::Move { t_ms, x, y });
                }
                _ => {}
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

pub struct MouseHook {
    timeline: std::sync::Arc<Mutex<Vec<InputEvent>>>,
    thread: Option<JoinHandle<Result<(), AirecError>>>,
    thread_id: u32,
}

impl MouseHook {
    pub fn install(started_at: Instant) -> Result<Self, AirecError> {
        let timeline = std::sync::Arc::new(Mutex::new(Vec::new()));
        let hook_timeline = timeline.clone();
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            if let Ok(mut state) = HOOK_STATE
                .get_or_init(|| Mutex::new(HookState::default()))
                .lock()
            {
                state.timeline = Some(hook_timeline);
                state.started_at = Some(started_at);
            }
            let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) }
                .map_err(|error| {
                    AirecError::new(
                        ErrorCode::CaptureInitFailed,
                        error.to_string(),
                        serde_json::json!({"component": "mouse_hook"}),
                    )
                })?;
            ready_sender.send(Ok(thread_id)).ok();
            run_message_loop(hook);
            Ok(())
        });
        let thread_id = ready_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| {
                AirecError::new(
                    ErrorCode::CaptureInitFailed,
                    error.to_string(),
                    serde_json::json!({"component": "mouse_hook"}),
                )
            })??;
        Ok(Self {
            timeline,
            thread: Some(thread),
            thread_id,
        })
    }

    #[must_use]
    pub fn subscribe(&self) -> MouseSubscription {
        MouseSubscription {
            timeline: self.timeline.clone(),
            cursor: 0,
        }
    }
}

fn run_message_loop(hook: HHOOK) {
    let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
    if let Ok(mut state) = HOOK_STATE
        .get_or_init(|| Mutex::new(HookState::default()))
        .lock()
    {
        state.timeline = None;
        state.started_at = None;
    }
}

impl Drop for MouseHook {
    fn drop(&mut self) {
        let _ = unsafe {
            PostThreadMessageW(
                self.thread_id,
                WM_QUIT,
                WPARAM::default(),
                LPARAM::default(),
            )
        };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct MouseSubscription {
    timeline: std::sync::Arc<Mutex<Vec<InputEvent>>>,
    cursor: usize,
}

impl InputSource for MouseSubscription {
    fn drain_until(&mut self, t_ms: u64) -> Vec<InputEvent> {
        let timeline = self
            .timeline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = &timeline[self.cursor..];
        let count = remaining.partition_point(|event| event_time(event) <= t_ms);
        let events = remaining[..count].to_vec();
        self.cursor += count;
        events
    }
}

fn push_event(timeline: &Mutex<Vec<InputEvent>>, event: InputEvent) {
    timeline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(event);
}

fn event_time(event: &InputEvent) -> u64 {
    match event {
        InputEvent::Click { t_ms, .. }
        | InputEvent::DragStart { t_ms, .. }
        | InputEvent::DragEnd { t_ms, .. }
        | InputEvent::Move { t_ms, .. } => *t_ms,
    }
}

pub struct EventLogWriter {
    writer: BufWriter<File>,
}

impl EventLogWriter {
    pub fn create(path: &Path) -> Result<Self, AirecError> {
        let file = File::create(path).map_err(|error| {
            AirecError::new(
                ErrorCode::OutputIoError,
                error.to_string(),
                serde_json::json!({"path": path}),
            )
        })?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    pub fn write(&mut self, event: &InputEvent) -> Result<(), AirecError> {
        serde_json::to_writer(&mut self.writer, event).map_err(io_error)?;
        self.writer.write_all(b"\n").map_err(io_error)?;
        self.writer.flush().map_err(io_error)
    }
}

fn io_error(error: impl std::fmt::Display) -> AirecError {
    AirecError::new(
        ErrorCode::OutputIoError,
        error.to_string(),
        serde_json::json!({"component": "event_log"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_schema_never_contains_keyboard_data() {
        let events = [
            InputEvent::Click {
                t_ms: 42,
                button: MouseButton::Left,
                x: 10,
                y: 20,
                double: false,
            },
            InputEvent::Move {
                t_ms: 100,
                x: 11,
                y: 21,
            },
        ];
        for event in events {
            let value = serde_json::to_value(event).unwrap();
            assert!(value.get("key").is_none());
            assert!(value.get("text").is_none());
            assert!(value.get("t_ms").is_some());
        }
    }

    #[test]
    fn click_timestamp_is_preserved_exactly() {
        let event = InputEvent::Click {
            t_ms: 1_234,
            button: MouseButton::Right,
            x: -4,
            y: 9,
            double: true,
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["t_ms"], 1_234);
        assert_eq!(value["button"], "right");
        assert_eq!(value["event"], "click");
    }
}
