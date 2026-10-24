//! Global mouse input on X11 through the RECORD extension, which copies every device event to
//! the recording client together with the pointer position at the time of that event.
//!
//! XInput2 raw events reach every client too, but carry no screen position; reading the pointer
//! after each one misplaces presses and drags that arrive faster than they are processed, as
//! input injected with XTEST does.

use std::thread::JoinHandle;

use airec_core::{AirecError, MouseButton};
use x11rb::connection::Connection;
use x11rb::protocol::record::{CS, ConnectionExt as _, Context, Range, Range8};
use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
use x11rb::rust_connection::RustConnection;

use crate::{HookState, MouseAction, hook_error};

/// X11 has no system-wide double-click setting; this matches the Windows default.
const DOUBLE_CLICK_MS: u64 = 500;
const LEFT_BUTTON: u8 = 1;
const RIGHT_BUTTON: u8 = 3;
/// Intercepted protocol data, as opposed to the start and end of a recording.
const CATEGORY_FROM_SERVER: u8 = 0;
const CORE_EVENT_LEN: usize = 32;
/// The event's position in root window coordinates.
const ROOT_X_OFFSET: usize = 20;
const ROOT_Y_OFFSET: usize = 22;

pub(crate) struct Listener {
    control: RustConnection,
    context: Context,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    pub(crate) fn start(mut state: HookState) -> Result<Self, AirecError> {
        let (control, _) =
            RustConnection::connect(None).map_err(|error| hook_error(error.to_string()))?;
        control
            .record_query_version(1, 13)
            .map_err(|error| hook_error(error.to_string()))?
            .reply()
            .map_err(|error| {
                hook_error(format!("the X RECORD extension is unavailable: {error}"))
            })?;
        let context = control
            .generate_id()
            .map_err(|error| hook_error(error.to_string()))?;
        let mouse_events = Range {
            device_events: Range8 {
                first: BUTTON_PRESS_EVENT,
                last: MOTION_NOTIFY_EVENT,
            },
            ..Range::default()
        };
        control
            .record_create_context(context, 0, &[CS::ALL_CLIENTS.into()], &[mouse_events])
            .map_err(|error| hook_error(error.to_string()))?
            .check()
            .map_err(|error| hook_error(error.to_string()))?;

        // An enabled context streams replies until it is disabled, so it needs its own connection.
        let (data, _) =
            RustConnection::connect(None).map_err(|error| hook_error(error.to_string()))?;
        let thread = std::thread::spawn(move || {
            let replies = match data.record_enable_context(context) {
                Ok(replies) => replies,
                Err(error) => {
                    eprintln!("warning: X11 mouse input stopped: {error}");
                    return;
                }
            };
            for reply in replies {
                let reply = match reply {
                    Ok(reply) => reply,
                    Err(error) => {
                        eprintln!("warning: X11 mouse input stopped: {error}");
                        return;
                    }
                };
                if reply.category != CATEGORY_FROM_SERVER {
                    continue;
                }
                for event in reply.data.chunks_exact(CORE_EVENT_LEN) {
                    if let Some((action, x, y)) = mouse_event(event) {
                        state.handle(action, x, y, DOUBLE_CLICK_MS);
                    }
                }
            }
        });
        Ok(Self {
            control,
            context,
            thread: Some(thread),
        })
    }
}

fn mouse_event(event: &[u8]) -> Option<(MouseAction, i32, i32)> {
    let action = match event[0] & 0x7f {
        BUTTON_PRESS_EVENT => MouseAction::Press(mouse_button(event[1])?),
        BUTTON_RELEASE_EVENT => MouseAction::Release(mouse_button(event[1])?),
        MOTION_NOTIFY_EVENT => MouseAction::Move,
        _ => return None,
    };
    let coordinate =
        |offset: usize| i32::from(i16::from_ne_bytes([event[offset], event[offset + 1]]));
    Some((action, coordinate(ROOT_X_OFFSET), coordinate(ROOT_Y_OFFSET)))
}

/// Buttons 4 through 7 are scroll wheel steps and 2 is the middle button; neither is drawn.
fn mouse_button(detail: u8) -> Option<MouseButton> {
    match detail {
        LEFT_BUTTON => Some(MouseButton::Left),
        RIGHT_BUTTON => Some(MouseButton::Right),
        _ => None,
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Disabling the context ends the reply stream, which lets the thread finish.
        let disabled = self
            .control
            .record_disable_context(self.context)
            .ok()
            .and_then(|cookie| cookie.check().ok());
        let _ = self.control.record_free_context(self.context);
        let _ = self.control.flush();
        if let Some(thread) = self.thread.take()
            && disabled.is_some()
        {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_event(kind: u8, detail: u8, x: i16, y: i16) -> [u8; CORE_EVENT_LEN] {
        let mut event = [0; CORE_EVENT_LEN];
        event[0] = kind;
        event[1] = detail;
        event[ROOT_X_OFFSET..ROOT_X_OFFSET + 2].copy_from_slice(&x.to_ne_bytes());
        event[ROOT_Y_OFFSET..ROOT_Y_OFFSET + 2].copy_from_slice(&y.to_ne_bytes());
        event
    }

    #[test]
    fn recorded_events_keep_the_position_they_happened_at() {
        let press = core_event(BUTTON_PRESS_EVENT, 1, 500, 300);
        let motion = core_event(MOTION_NOTIFY_EVENT, 0, -20, 1079);

        assert_eq!(
            mouse_event(&press),
            Some((MouseAction::Press(MouseButton::Left), 500, 300))
        );
        assert_eq!(mouse_event(&motion), Some((MouseAction::Move, -20, 1079)));
    }

    #[test]
    fn scroll_and_middle_buttons_are_not_mouse_clicks() {
        assert_eq!(mouse_button(1), Some(MouseButton::Left));
        assert_eq!(mouse_button(3), Some(MouseButton::Right));
        for detail in [2, 4, 5, 6, 7] {
            assert_eq!(mouse_button(detail), None);
        }
        let scroll = core_event(BUTTON_PRESS_EVENT, 4, 0, 0);
        assert_eq!(mouse_event(&scroll), None);
    }
}
