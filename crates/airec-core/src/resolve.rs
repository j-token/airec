use serde::{Deserialize, Serialize};

use crate::{AirecError, ErrorCode, FailurePolicy, StopReason, WindowInfo};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SuccessRecord {
    pub target: String,
    pub file: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FailureRecord {
    pub target: String,
    pub code: ErrorCode,
    pub message: String,
}

pub fn match_title<'a>(windows: &'a [WindowInfo], query: &str) -> Result<&'a WindowInfo, AirecError> {
    let candidates: Vec<_> = windows.iter().filter(|window| window.title.contains(query)).collect();
    match candidates.as_slice() {
        [] => Err(AirecError::new(
            ErrorCode::TargetNotFound,
            format!("no window matches title '{query}'"),
            serde_json::json!({"query": query, "candidates": []}),
        )),
        [candidate] => Ok(candidate),
        _ => Err(AirecError::new(
            ErrorCode::AmbiguousTarget,
            format!("more than one window matches title '{query}'"),
            serde_json::json!({"query": query, "candidates": candidates}),
        )),
    }
}

pub fn match_process<'a>(windows: &'a [WindowInfo], query: &str) -> Result<Vec<&'a WindowInfo>, AirecError> {
    let matches: Vec<_> = windows.iter().filter(|window| window.process.eq_ignore_ascii_case(query)).collect();
    if matches.is_empty() {
        Err(AirecError::new(
            ErrorCode::TargetNotFound,
            format!("no window belongs to process '{query}'"),
            serde_json::json!({"query": query, "candidates": []}),
        ))
    } else {
        Ok(matches)
    }
}

#[must_use]
pub fn aggregate_failures(
    policy: FailurePolicy,
    successes: &[SuccessRecord],
    failures: &[FailureRecord],
) -> Option<AirecError> {
    if failures.is_empty() {
        return None;
    }
    let (code, message, stop_reason) = match policy {
        FailurePolicy::Continue => (ErrorCode::PartialFailure, "one or more targets failed", StopReason::Error),
        FailurePolicy::Abort => (
            ErrorCode::AbortedOnFailure,
            "session aborted because a target failed",
            StopReason::AbortedOnFailure,
        ),
    };
    Some(AirecError::new(
        code,
        message,
        serde_json::json!({"successes": successes, "failures": failures, "stop_reason": stop_reason}),
    ))
}

#[must_use]
pub fn sanitize_file_stem(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_dash = false;
    for character in value.chars() {
        let mapped = if character.is_alphanumeric() { character } else { '-' };
        if mapped == '-' {
            if !last_dash && !output.is_empty() {
                output.push(mapped);
            }
            last_dash = true;
        } else {
            output.extend(mapped.to_lowercase());
            last_dash = false;
        }
    }
    output.trim_matches('-').chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(hwnd: isize, title: &str, process: &str) -> WindowInfo {
        WindowInfo { hwnd, title: title.into(), process: process.into(), pid: 1, width: 800, height: 600, minimized: false }
    }

    #[test]
    fn ambiguous_title_has_candidates_and_exit_two() {
        let windows = [window(1, "MyApp - A", "app.exe"), window(2, "MyApp - B", "app.exe")];
        let error = match_title(&windows, "MyApp").unwrap_err();
        assert_eq!(error.code, ErrorCode::AmbiguousTarget);
        assert_eq!(error.exit_code(), 2);
        assert_eq!(error.data["candidates"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn process_matching_is_case_insensitive_and_can_return_multiple() {
        let windows = [window(1, "A", "APP.EXE"), window(2, "B", "app.exe")];
        assert_eq!(match_process(&windows, "App.exe").unwrap().len(), 2);
    }

    #[test]
    fn failure_policy_maps_to_exact_aggregate_error() {
        let failures = [FailureRecord { target: "monitor-2".into(), code: ErrorCode::CaptureInitFailed, message: "no WGC".into() }];
        let continued = aggregate_failures(FailurePolicy::Continue, &[], &failures).unwrap();
        assert_eq!(continued.code, ErrorCode::PartialFailure);
        assert_eq!(continued.exit_code(), 6);
        let aborted = aggregate_failures(FailurePolicy::Abort, &[], &failures).unwrap();
        assert_eq!(aborted.code, ErrorCode::AbortedOnFailure);
        assert_eq!(aborted.data["stop_reason"], "aborted_on_failure");
    }

    #[test]
    fn filename_sanitization_is_windows_safe() {
        assert_eq!(sanitize_file_stem(" My App: Settings / β "), "my-app-settings-β");
        assert_eq!(sanitize_file_stem("<>:\"/\\|?*"), "");
    }
}
