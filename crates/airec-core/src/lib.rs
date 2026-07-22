//! Front-end-neutral contracts and session orchestration for airec.

mod error;
mod event;
mod model;
mod protocol;
mod resolve;
mod session;

pub use error::{AirecError, ErrorCode};
pub use event::{Event, EventTarget, StopReason, TargetSummary, Timestamp};
pub use model::{
    CaptureBackend, CaptureFrame, CaptureTarget, ClickEffectStyle, DEFAULT_CLICK_DURATION_MS,
    DEFAULT_CLICK_SIZE, DEFAULT_DRAG_COLOR, DEFAULT_DRAG_DURATION_MS, DEFAULT_DRAG_SIZE,
    DEFAULT_DRAG_THRESHOLD_MS, DEFAULT_DRAG_THRESHOLD_PIXELS, DEFAULT_LEFT_CLICK_COLOR,
    DEFAULT_MAX_INPUT_HISTORY_EVENTS, DEFAULT_MOVE_THROTTLE_MS, DEFAULT_RIGHT_CLICK_COLOR,
    DEFAULT_TRAIL_COLOR, DEFAULT_TRAIL_DURATION_MS, DEFAULT_TRAIL_SIZE, EffectColor, EffectStyles,
    EncoderFactory, FIRST_FRAME_TIMEOUT, FailurePolicy, FrameProcessor, FrameSource, InputEvent,
    InputOptions, InputSource, LineEffectStyle, MonitorInfo, MouseButton, PipelineEncoder, Quality,
    RecordingOptions, ResolvedTarget, TargetCatalog, TargetKind, WindowInfo,
};
pub use protocol::{
    ControlRequest, ControlResponse, SessionSnapshot, SessionState, TargetSnapshot,
};
pub use resolve::{
    FailureRecord, SuccessRecord, aggregate_failures, match_process, match_title,
    sanitize_file_stem,
};
pub use session::{Pipeline, SessionOutcome, SessionRunOptions, run_session};
