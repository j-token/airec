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
    CaptureBackend, CaptureFrame, CaptureTarget, EncoderFactory, FIRST_FRAME_TIMEOUT,
    FailurePolicy, FrameProcessor, FrameSource, InputEvent, InputSource, MonitorInfo, MouseButton,
    PipelineEncoder, Quality, RecordingOptions, ResolvedTarget, TargetCatalog, TargetKind,
    WindowInfo,
};
pub use protocol::{
    ControlRequest, ControlResponse, SessionSnapshot, SessionState, TargetSnapshot,
};
pub use resolve::{
    FailureRecord, SuccessRecord, aggregate_failures, match_process, match_title,
    sanitize_file_stem,
};
pub use session::{Pipeline, SessionOutcome, SessionRunOptions, run_session};
