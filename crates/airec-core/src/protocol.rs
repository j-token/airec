use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, Event, StopReason};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ControlRequest {
    Status,
    Stop,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum ControlResponse {
    Status { session: SessionSnapshot },
    Events { events: Vec<Event>, exit_code: i32 },
    Error { code: ErrorCode, message: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Recording,
    Stopping,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TargetSnapshot {
    pub target: String,
    pub file: PathBuf,
    pub elapsed_ms: u64,
    pub frames: u64,
    pub dropped: u64,
    pub minimized: bool,
    pub stop_reason: Option<StopReason>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub session: String,
    pub state: SessionState,
    pub pipe: String,
    pub targets: Vec<TargetSnapshot>,
    pub updated_at: String,
}
