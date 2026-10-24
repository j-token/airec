//! `WH_MOUSE_LL` low-level mouse hook running on its own message-loop thread.

use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use airec_core::{AirecError, MouseButton};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, MSLLHOOKSTRUCT, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

use crate::{HookState, MouseAction, hook_error};

// The hook procedure receives no user data pointer, so its state has to be global.
static HOOK_STATE: OnceLock<Mutex<HookState>> = OnceLock::new();

fn hook_state() -> &'static Mutex<HookState> {
    HOOK_STATE.get_or_init(|| Mutex::new(HookState::default()))
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let action = match wparam.0 as u32 {
            WM_LBUTTONDOWN => Some(MouseAction::Press(MouseButton::Left)),
            WM_RBUTTONDOWN => Some(MouseAction::Press(MouseButton::Right)),
            WM_LBUTTONUP => Some(MouseAction::Release(MouseButton::Left)),
            WM_RBUTTONUP => Some(MouseAction::Release(MouseButton::Right)),
            WM_MOUSEMOVE => Some(MouseAction::Move),
            _ => None,
        };
        if let Some(action) = action
            && let Ok(mut state) = hook_state().lock()
        {
            let double_click_ms = u64::from(unsafe { GetDoubleClickTime() });
            state.handle(action, data.pt.x, data.pt.y, double_click_ms);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

pub(crate) struct Listener {
    thread: Option<JoinHandle<Result<(), AirecError>>>,
    thread_id: u32,
}

impl Listener {
    pub(crate) fn start(state: HookState) -> Result<Self, AirecError> {
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            if let Ok(mut current) = hook_state().lock() {
                *current = state;
            }
            let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) }
                .map_err(|error| hook_error(error.to_string()))?;
            ready_sender.send(Ok(thread_id)).ok();
            run_message_loop(hook);
            Ok(())
        });
        let thread_id = ready_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| hook_error(error.to_string()))??;
        Ok(Self {
            thread: Some(thread),
            thread_id,
        })
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
    if let Ok(mut state) = hook_state().lock() {
        *state = HookState::default();
    }
}

impl Drop for Listener {
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
