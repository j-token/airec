use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ErrorCode;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Timestamp(String);

impl Timestamp {
    #[must_use]
    pub fn now() -> Self {
        Self(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    #[must_use]
    pub fn from_datetime(value: DateTime<Utc>) -> Self {
        Self(value.to_rfc3339_opts(SecondsFormat::Millis, true))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Requested,
    DurationLimit,
    MaxDuration,
    TargetLost,
    Error,
    AbortedOnFailure,
}

impl StopReason {
    #[must_use]
    pub const fn is_intended(self) -> bool {
        matches!(
            self,
            Self::Requested | Self::DurationLimit | Self::MaxDuration
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTarget {
    File(String),
    Named(String),
}

impl std::fmt::Display for EventTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(value) | Self::Named(value) => formatter.write_str(value),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TargetSummary {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub file: String,
}

/// The exact stdout JSONL contract. Session-wide events serialize `target: null`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Started {
        ts: Timestamp,
        session: String,
        target: Option<String>,
        targets: Vec<TargetSummary>,
    },
    Heartbeat {
        ts: Timestamp,
        session: String,
        target: String,
        elapsed_ms: u64,
        frames: u64,
        dropped: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimized: Option<bool>,
    },
    TargetLost {
        ts: Timestamp,
        session: String,
        target: String,
        stop_reason: StopReason,
        data: Value,
    },
    Saved {
        ts: Timestamp,
        session: String,
        target: String,
        file: PathBuf,
        stop_reason: StopReason,
        duration_ms: u64,
        frames: u64,
        size_bytes: u64,
    },
    Error {
        ts: Timestamp,
        session: String,
        target: Option<String>,
        code: ErrorCode,
        message: String,
        data: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
    },
}

impl Event {
    pub fn write_jsonl(&self, mut writer: impl std::io::Write) -> std::io::Result<()> {
        serde_json::to_writer(&mut writer, self)?;
        writer.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn ts() -> Timestamp {
        Timestamp::from_datetime(Utc.with_ymd_and_hms(2026, 7, 22, 10, 0, 0).unwrap())
    }

    #[test]
    fn every_stop_reason_has_stable_wire_value_and_classification() {
        let cases = [
            (StopReason::Requested, "requested", true),
            (StopReason::DurationLimit, "duration_limit", true),
            (StopReason::MaxDuration, "max_duration", true),
            (StopReason::TargetLost, "target_lost", false),
            (StopReason::Error, "error", false),
            (StopReason::AbortedOnFailure, "aborted_on_failure", false),
        ];
        for (reason, wire, intended) in cases {
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(reason.is_intended(), intended);
        }
    }

    #[test]
    fn started_has_common_envelope_and_targets() {
        let event = Event::Started {
            ts: ts(),
            session: "a1b2".into(),
            target: None,
            targets: vec![TargetSummary {
                kind: "window".into(),
                title: Some("MyApp".into()),
                file: "evidence.mp4".into(),
            }],
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "started");
        assert_eq!(value["ts"], "2026-07-22T10:00:00.000Z");
        assert_eq!(value["session"], "a1b2");
        assert!(value["target"].is_null());
        assert_eq!(value["targets"][0]["type"], "window");
    }

    #[test]
    fn each_event_is_exactly_one_json_line() {
        let event = Event::Error {
            ts: ts(),
            session: "none".into(),
            target: None,
            code: ErrorCode::TargetNotFound,
            message: "not found".into(),
            data: serde_json::json!({"candidates": []}),
            stop_reason: None,
        };
        let mut bytes = Vec::new();
        event.write_jsonl(&mut bytes).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        serde_json::from_slice::<Value>(&bytes[..bytes.len() - 1]).unwrap();
    }
}
