//! Mouse-only low-level input collection. This crate intentionally has no keyboard APIs.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use airec_core::{AirecError, ErrorCode, InputEvent, InputSource, MouseButton};
pub use airec_core::{
    DEFAULT_DRAG_THRESHOLD_MS, DEFAULT_DRAG_THRESHOLD_PIXELS,
    DEFAULT_MAX_INPUT_HISTORY_EVENTS as DEFAULT_MAX_HISTORY_EVENTS, DEFAULT_MOVE_THROTTLE_MS,
    InputOptions as MouseHookOptions,
};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, MSLLHOOKSTRUCT, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

#[derive(Default)]
struct EventTimeline {
    events: VecDeque<InputEvent>,
    base_index: u64,
}

#[derive(Clone, Copy, Debug)]
struct PressedButton {
    button: MouseButton,
    x: i32,
    y: i32,
    t_ms: u64,
}

#[derive(Default)]
struct HookState {
    timeline: Option<std::sync::Arc<Mutex<EventTimeline>>>,
    started_at: Option<Instant>,
    last_move_ms: u64,
    last_click: Option<(MouseButton, u64, i32, i32)>,
    pressed: Option<PressedButton>,
    drag_active: bool,
    options: MouseHookOptions,
}

impl HookState {
    fn press(&mut self, button: MouseButton, t_ms: u64, x: i32, y: i32) {
        self.pressed = Some(PressedButton { button, x, y, t_ms });
        self.drag_active = false;
    }

    fn move_events(&mut self, t_ms: u64, x: i32, y: i32) -> Vec<InputEvent> {
        let mut events = Vec::with_capacity(2);
        if let Some(pressed) = self.pressed
            && !self.drag_active
        {
            let dx = x.abs_diff(pressed.x);
            let dy = y.abs_diff(pressed.y);
            let moved = dx != 0 || dy != 0;
            let pixel_threshold_exceeded =
                dx > self.options.drag_threshold_pixels || dy > self.options.drag_threshold_pixels;
            let time_threshold_reached =
                moved && t_ms.saturating_sub(pressed.t_ms) >= self.options.drag_threshold_ms;
            if pixel_threshold_exceeded || time_threshold_reached {
                self.drag_active = true;
                events.push(InputEvent::DragStart {
                    t_ms,
                    button: pressed.button,
                    x: pressed.x,
                    y: pressed.y,
                });
            }
        }
        if t_ms.saturating_sub(self.last_move_ms) >= self.options.move_throttle_ms {
            self.last_move_ms = t_ms;
            events.push(InputEvent::Move { t_ms, x, y });
        }
        events
    }
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
                    state.press(button, t_ms, x, y);
                }
                WM_LBUTTONUP | WM_RBUTTONUP => {
                    let button = if message == WM_LBUTTONUP {
                        MouseButton::Left
                    } else {
                        MouseButton::Right
                    };
                    let was_drag = state.drag_active;
                    if was_drag {
                        push_event(
                            &timeline,
                            InputEvent::DragEnd { t_ms, button, x, y },
                            state.options.max_history_events,
                        );
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
                            state.options.max_history_events,
                        );
                    }
                }
                WM_MOUSEMOVE => {
                    let max_history_events = state.options.max_history_events;
                    for event in state.move_events(t_ms, x, y) {
                        push_event(&timeline, event, max_history_events);
                    }
                }
                _ => {}
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

pub struct MouseHook {
    timeline: std::sync::Arc<Mutex<EventTimeline>>,
    thread: Option<JoinHandle<Result<(), AirecError>>>,
    thread_id: u32,
}

impl MouseHook {
    pub fn install(started_at: Instant) -> Result<Self, AirecError> {
        Self::install_with_options(started_at, MouseHookOptions::default())
    }

    pub fn install_with_options(
        started_at: Instant,
        options: MouseHookOptions,
    ) -> Result<Self, AirecError> {
        let timeline = std::sync::Arc::new(Mutex::new(EventTimeline::default()));
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
                state.last_move_ms = 0;
                state.last_click = None;
                state.pressed = None;
                state.drag_active = false;
                state.options = options;
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
        let cursor = self
            .timeline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .base_index;
        MouseSubscription {
            timeline: self.timeline.clone(),
            cursor,
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
    timeline: std::sync::Arc<Mutex<EventTimeline>>,
    cursor: u64,
}

impl InputSource for MouseSubscription {
    fn drain_until(&mut self, t_ms: u64) -> Vec<InputEvent> {
        let timeline = self
            .timeline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cursor = self.cursor.max(timeline.base_index);
        let offset = usize::try_from(self.cursor - timeline.base_index).unwrap_or(usize::MAX);
        let events: Vec<_> = timeline
            .events
            .iter()
            .skip(offset)
            .take_while(|event| event_time(event) <= t_ms)
            .cloned()
            .collect();
        self.cursor = self.cursor.saturating_add(events.len() as u64);
        events
    }
}

fn push_event(timeline: &Mutex<EventTimeline>, event: InputEvent, max_history_events: usize) {
    let mut timeline = timeline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    timeline.events.push_back(event);
    let limit = max_history_events.max(1);
    while timeline.events.len() > limit {
        timeline.events.pop_front();
        timeline.base_index = timeline.base_index.saturating_add(1);
    }
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

    fn move_event(t_ms: u64) -> InputEvent {
        InputEvent::Move {
            t_ms,
            x: t_ms as i32,
            y: 0,
        }
    }

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

    #[test]
    fn fast_drag_is_detected_even_when_move_is_throttled() {
        let mut state = HookState {
            options: MouseHookOptions {
                move_throttle_ms: 100,
                ..MouseHookOptions::default()
            },
            ..HookState::default()
        };
        state.press(MouseButton::Left, 0, 0, 0);

        let first = state.move_events(10, 5, 0);
        assert!(matches!(
            first.as_slice(),
            [InputEvent::DragStart {
                t_ms: 10,
                x: 0,
                y: 0,
                ..
            }]
        ));
        assert!(state.move_events(20, 10, 0).is_empty());
    }

    #[test]
    fn pixel_and_time_drag_thresholds_are_configurable() {
        let mut state = HookState {
            options: MouseHookOptions {
                drag_threshold_pixels: 10,
                drag_threshold_ms: 50,
                move_throttle_ms: 1_000,
                ..MouseHookOptions::default()
            },
            ..HookState::default()
        };
        state.press(MouseButton::Right, 0, 100, 100);
        assert!(state.move_events(49, 101, 100).is_empty());
        assert!(matches!(
            state.move_events(50, 101, 100).as_slice(),
            [InputEvent::DragStart {
                button: MouseButton::Right,
                ..
            }]
        ));

        let mut pixel_state = HookState::default();
        pixel_state.press(MouseButton::Left, 0, 0, 0);
        assert!(pixel_state.move_events(1, 4, 0).is_empty());
        assert!(matches!(
            pixel_state.move_events(2, 5, 0).as_slice(),
            [InputEvent::DragStart { .. }]
        ));
    }

    #[test]
    fn bounded_history_keeps_subscriptions_safe_and_ordered() {
        let timeline = std::sync::Arc::new(Mutex::new(EventTimeline::default()));
        for t_ms in 0..6 {
            push_event(&timeline, move_event(t_ms), 4);
        }
        {
            let timeline = timeline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(timeline.events.len(), 4);
            assert_eq!(timeline.base_index, 2);
        }

        let mut lagging = MouseSubscription {
            timeline,
            cursor: 0,
        };
        let events = lagging.drain_until(u64::MAX);
        assert_eq!(
            events.iter().map(event_time).collect::<Vec<_>>(),
            [2, 3, 4, 5]
        );
        assert!(lagging.drain_until(u64::MAX).is_empty());
    }
}
