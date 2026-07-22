use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Stable machine-readable errors from PRD §13.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    TargetNotFound,
    AmbiguousTarget,
    CaptureInitFailed,
    EncoderUnavailable,
    FirstFrameTimeout,
    NoActiveSession,
    SessionAmbiguous,
    OutputIoError,
    PartialFailure,
    AbortedOnFailure,
    PermissionDenied,
    TargetLost,
}

impl ErrorCode {
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::TargetNotFound | Self::AmbiguousTarget => 2,
            Self::CaptureInitFailed | Self::EncoderUnavailable | Self::FirstFrameTimeout => 3,
            Self::NoActiveSession | Self::SessionAmbiguous => 4,
            Self::OutputIoError => 5,
            Self::PartialFailure | Self::AbortedOnFailure => 6,
            Self::PermissionDenied => 7,
            Self::TargetLost => 8,
        }
    }
}

#[derive(Clone, Debug, Error)]
#[error("{message}")]
pub struct AirecError {
    pub code: ErrorCode,
    pub message: String,
    pub data: Value,
}

impl AirecError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prd_error_exit_codes_are_stable() {
        let cases = [
            (ErrorCode::TargetNotFound, 2),
            (ErrorCode::AmbiguousTarget, 2),
            (ErrorCode::CaptureInitFailed, 3),
            (ErrorCode::EncoderUnavailable, 3),
            (ErrorCode::FirstFrameTimeout, 3),
            (ErrorCode::NoActiveSession, 4),
            (ErrorCode::SessionAmbiguous, 4),
            (ErrorCode::OutputIoError, 5),
            (ErrorCode::PartialFailure, 6),
            (ErrorCode::AbortedOnFailure, 6),
            (ErrorCode::PermissionDenied, 7),
            (ErrorCode::TargetLost, 8),
        ];
        for (code, expected) in cases {
            assert_eq!(code.exit_code(), expected);
        }
    }

    #[test]
    fn code_serialization_matches_contract() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::FirstFrameTimeout).unwrap(),
            "\"FIRST_FRAME_TIMEOUT\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::TargetLost).unwrap(),
            "\"TARGET_LOST\""
        );
    }
}
