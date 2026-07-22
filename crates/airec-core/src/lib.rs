//! Front-end-neutral contracts and session orchestration for airec.

mod error;
mod event;
mod model;
mod protocol;
mod resolve;

pub use error::{AirecError, ErrorCode};
pub use event::{Event, EventTarget, StopReason, TargetSummary, Timestamp};
pub use model::{
    CaptureBackend, CaptureFrame, CaptureTarget, EncoderFactory, FailurePolicy, FrameSource, InputEvent, InputSource,
    MonitorInfo, MouseButton, PipelineEncoder, Quality, RecordingOptions, ResolvedTarget, TargetCatalog, TargetKind,
    WindowInfo, FIRST_FRAME_TIMEOUT,
};
pub use protocol::{ControlRequest, ControlResponse, SessionSnapshot, SessionState, TargetSnapshot};
pub use resolve::{FailureRecord, SuccessRecord, aggregate_failures, match_process, match_title, sanitize_file_stem};
