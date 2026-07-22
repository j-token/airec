use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::{
    AirecError, ErrorCode, Event, FailurePolicy, FailureRecord, FrameProcessor, FrameSource,
    PipelineEncoder, StopReason, SuccessRecord, TargetSummary, Timestamp, aggregate_failures,
};

pub struct Pipeline {
    pub target: String,
    pub kind: String,
    pub title: Option<String>,
    pub output: std::path::PathBuf,
    pub source: Box<dyn FrameSource>,
    pub encoder: Box<dyn PipelineEncoder>,
    pub processor: Box<dyn FrameProcessor>,
}

#[derive(Clone)]
pub struct SessionRunOptions {
    pub session: String,
    pub fps: u32,
    pub first_frame_timeout: Duration,
    pub duration: Option<Duration>,
    pub max_duration: Duration,
    pub heartbeat_interval: Duration,
    pub failure_policy: FailurePolicy,
    /// A controller sets this to `Some(Requested)` for stop/Ctrl+C.
    pub stop: Arc<Mutex<Option<StopReason>>>,
}

#[derive(Clone, Debug)]
pub struct SessionOutcome {
    pub events: Vec<Event>,
    pub exit_code: i32,
    pub error: Option<AirecError>,
}

enum PipelineMessage {
    FirstFrame(TargetSummary),
    Event(Event),
    Failure(FailureRecord),
    Complete(SuccessRecord),
}

pub fn run_session(
    pipelines: Vec<Pipeline>,
    options: SessionRunOptions,
    mut emit: impl FnMut(&Event),
) -> SessionOutcome {
    if pipelines.is_empty() {
        let error = AirecError::new(
            ErrorCode::TargetNotFound,
            "no capture targets",
            json!({"candidates": []}),
        );
        let event = error_event(&options.session, None, &error, None);
        emit(&event);
        return SessionOutcome {
            events: vec![event],
            exit_code: error.exit_code(),
            error: Some(error),
        };
    }
    let target_count = pipelines.len();
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut handles = Vec::with_capacity(target_count);
    for pipeline in pipelines {
        let sender = sender.clone();
        let thread_options = options.clone();
        handles.push(std::thread::spawn(move || {
            run_pipeline(pipeline, thread_options, sender)
        }));
    }
    drop(sender);

    let mut events = Vec::new();
    let mut started_targets = Vec::new();
    let mut successes = Vec::new();
    let mut failures = Vec::new();
    let mut completed = 0;
    let mut started_emitted = false;
    let mut startup_resolved = 0;
    let mut startup_targets = std::collections::HashSet::new();

    while completed < target_count {
        let Ok(message) = receiver.recv() else { break };
        match message {
            PipelineMessage::FirstFrame(target) => {
                if startup_targets.insert(target.file.clone()) {
                    startup_resolved += 1;
                }
                started_targets.push(target);
            }
            PipelineMessage::Event(event) => {
                emit(&event);
                events.push(event);
            }
            PipelineMessage::Failure(failure) => {
                if startup_targets.insert(failure.target.clone()) {
                    startup_resolved += 1;
                }
                failures.push(failure);
                completed += 1;
                if options.failure_policy == FailurePolicy::Abort {
                    set_stop(&options.stop, StopReason::AbortedOnFailure);
                }
            }
            PipelineMessage::Complete(success) => {
                successes.push(success);
                completed += 1;
            }
        }
        if !started_emitted && startup_resolved == target_count && !started_targets.is_empty() {
            let event = Event::Started {
                ts: Timestamp::now(),
                session: options.session.clone(),
                target: None,
                targets: started_targets.clone(),
            };
            emit(&event);
            events.push(event);
            started_emitted = true;
        }
    }
    for handle in handles {
        if handle.join().is_err() {
            failures.push(FailureRecord {
                target: "pipeline-thread".into(),
                code: ErrorCode::CaptureInitFailed,
                message: "pipeline thread panicked".into(),
            });
        }
    }

    let error = if failures.is_empty() {
        None
    } else if successes.is_empty()
        && (target_count == 1 || options.failure_policy == FailurePolicy::Continue)
    {
        let failure = &failures[0];
        Some(AirecError::new(
            failure.code,
            failure.message.clone(),
            json!({"target": failure.target, "failures": failures}),
        ))
    } else {
        aggregate_failures(options.failure_policy, &successes, &failures)
    };
    if let Some(error) = &error {
        let stop_reason = match error.code {
            ErrorCode::AbortedOnFailure => Some(StopReason::AbortedOnFailure),
            _ => Some(StopReason::Error),
        };
        let event = error_event(&options.session, None, error, stop_reason);
        emit(&event);
        events.push(event);
    }
    let exit_code = error.as_ref().map_or(0, AirecError::exit_code);
    SessionOutcome {
        events,
        exit_code,
        error,
    }
}

fn run_pipeline(
    mut pipeline: Pipeline,
    options: SessionRunOptions,
    sender: std::sync::mpsc::Sender<PipelineMessage>,
) {
    let first = match pipeline.source.next_frame(options.first_frame_timeout) {
        Ok(Some(frame)) => frame,
        Ok(None) if !pipeline.source.is_target_alive() => {
            pipeline_failure(
                &pipeline,
                &options,
                ErrorCode::CaptureInitFailed,
                "target was lost before the first frame",
                StopReason::TargetLost,
                &sender,
            );
            return;
        }
        Ok(None) => {
            pipeline_failure(
                &pipeline,
                &options,
                ErrorCode::FirstFrameTimeout,
                "first frame did not arrive within 30 seconds",
                StopReason::Error,
                &sender,
            );
            return;
        }
        Err(error) => {
            pipeline_failure(
                &pipeline,
                &options,
                error.code,
                &error.message,
                StopReason::Error,
                &sender,
            );
            return;
        }
    };

    let _ = sender.send(PipelineMessage::FirstFrame(TargetSummary {
        kind: pipeline.kind.clone(),
        title: pipeline.title.clone(),
        file: pipeline.output.to_string_lossy().into_owned(),
    }));

    let session_started = Instant::now();
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(options.fps.max(1)));
    let mut next_frame_at = Instant::now();
    let mut next_heartbeat = Instant::now() + options.heartbeat_interval;
    let mut last_frame = first;
    let mut frames = 0_u64;
    let dropped = 0_u64;
    let stop_reason = loop {
        let elapsed = session_started.elapsed();
        let automatic_reason = if options.duration.is_some_and(|duration| elapsed >= duration) {
            Some(StopReason::DurationLimit)
        } else if elapsed >= options.max_duration {
            Some(StopReason::MaxDuration)
        } else {
            None
        };
        if let Some(reason) = read_stop(&options.stop).or(automatic_reason) {
            break reason;
        }
        if !pipeline.source.is_target_alive() {
            let event = Event::TargetLost {
                ts: Timestamp::now(),
                session: options.session.clone(),
                target: pipeline.target.clone(),
                stop_reason: StopReason::TargetLost,
                data: json!({"cause": "window_closed"}),
            };
            let _ = sender.send(PipelineMessage::Event(event));
            break StopReason::TargetLost;
        }

        next_frame_at += frame_interval;
        let wait = next_frame_at.saturating_duration_since(Instant::now());
        match pipeline.source.next_frame(wait) {
            Ok(Some(frame)) => last_frame = frame,
            Ok(None) => {}
            Err(error) => {
                pipeline_failure(
                    &pipeline,
                    &options,
                    error.code,
                    &error.message,
                    StopReason::Error,
                    &sender,
                );
                return;
            }
        }
        let mut paced_frame = last_frame.clone();
        paced_frame.t_ms = session_started.elapsed().as_millis() as u64;
        let processed = match pipeline.processor.process(paced_frame) {
            Ok(frame) => frame,
            Err(error) => {
                pipeline_failure(
                    &pipeline,
                    &options,
                    error.code,
                    &error.message,
                    StopReason::Error,
                    &sender,
                );
                return;
            }
        };
        if let Err(error) = pipeline.encoder.write_frame(&processed) {
            pipeline_failure(
                &pipeline,
                &options,
                error.code,
                &error.message,
                StopReason::Error,
                &sender,
            );
            return;
        }
        frames += 1;
        if Instant::now() >= next_heartbeat {
            let event = Event::Heartbeat {
                ts: Timestamp::now(),
                session: options.session.clone(),
                target: pipeline.target.clone(),
                elapsed_ms: session_started.elapsed().as_millis() as u64,
                frames,
                dropped,
                minimized: Some(pipeline.source.is_minimized()),
            };
            let _ = sender.send(PipelineMessage::Event(event));
            next_heartbeat += options.heartbeat_interval;
        }
    };

    if let Err(error) = pipeline.encoder.finish() {
        pipeline_failure(
            &pipeline,
            &options,
            error.code,
            &error.message,
            StopReason::Error,
            &sender,
        );
        return;
    }
    let size_bytes = std::fs::metadata(&pipeline.output).map_or(0, |metadata| metadata.len());
    let duration_ms = session_started.elapsed().as_millis() as u64;
    let event = Event::Saved {
        ts: Timestamp::now(),
        session: options.session.clone(),
        target: pipeline.target.clone(),
        file: pipeline.output.clone(),
        stop_reason,
        duration_ms,
        frames,
        size_bytes,
    };
    let _ = sender.send(PipelineMessage::Event(event));
    if stop_reason.is_intended() {
        let _ = sender.send(PipelineMessage::Complete(SuccessRecord {
            target: pipeline.target,
            file: pipeline.output.to_string_lossy().into_owned(),
        }));
    } else {
        let code = if stop_reason == StopReason::AbortedOnFailure {
            ErrorCode::AbortedOnFailure
        } else {
            ErrorCode::CaptureInitFailed
        };
        let _ = sender.send(PipelineMessage::Failure(FailureRecord {
            target: pipeline.target,
            code,
            message: format!("pipeline ended with {stop_reason:?}"),
        }));
    }
}

fn pipeline_failure(
    pipeline: &Pipeline,
    options: &SessionRunOptions,
    code: ErrorCode,
    message: &str,
    stop_reason: StopReason,
    sender: &std::sync::mpsc::Sender<PipelineMessage>,
) {
    let error = AirecError::new(code, message, json!({"target": pipeline.target}));
    let _ = sender.send(PipelineMessage::Event(error_event(
        &options.session,
        Some(pipeline.target.clone()),
        &error,
        Some(stop_reason),
    )));
    let _ = sender.send(PipelineMessage::Failure(FailureRecord {
        target: pipeline.target.clone(),
        code,
        message: message.into(),
    }));
}

fn error_event(
    session: &str,
    target: Option<String>,
    error: &AirecError,
    stop_reason: Option<StopReason>,
) -> Event {
    Event::Error {
        ts: Timestamp::now(),
        session: session.into(),
        target,
        code: error.code,
        message: error.message.clone(),
        data: error.data.clone(),
        stop_reason,
    }
}

fn read_stop(stop: &Mutex<Option<StopReason>>) -> Option<StopReason> {
    stop.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .copied()
}

fn set_stop(stop: &Mutex<Option<StopReason>>, reason: StopReason) {
    let mut value = stop
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if value.is_none() {
        *value = Some(reason);
    }
}

#[allow(dead_code)]
fn _path_as_target(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::CaptureFrame;

    struct MockSource {
        frames: VecDeque<Option<CaptureFrame>>,
        alive: bool,
    }

    impl FrameSource for MockSource {
        fn next_frame(&mut self, _timeout: Duration) -> Result<Option<CaptureFrame>, AirecError> {
            Ok(self
                .frames
                .pop_front()
                .unwrap_or_else(|| self.frames.back().cloned().unwrap_or(None)))
        }
        fn is_target_alive(&self) -> bool {
            self.alive
        }
        fn is_minimized(&self) -> bool {
            false
        }
    }

    struct MockEncoder;
    impl PipelineEncoder for MockEncoder {
        fn write_frame(&mut self, _frame: &CaptureFrame) -> Result<(), AirecError> {
            Ok(())
        }
        fn finish(&mut self) -> Result<(), AirecError> {
            Ok(())
        }
    }

    fn frame() -> CaptureFrame {
        CaptureFrame {
            width: 2,
            height: 2,
            stride: 8,
            bgra: vec![0; 16],
            t_ms: 0,
        }
    }

    fn pipeline(source: MockSource) -> Pipeline {
        Pipeline {
            target: "mock.mp4".into(),
            kind: "monitor".into(),
            title: None,
            output: "mock.mp4".into(),
            source: Box::new(source),
            encoder: Box::new(MockEncoder),
            processor: Box::new(()),
        }
    }

    fn options(duration: Duration) -> SessionRunOptions {
        SessionRunOptions {
            session: "test".into(),
            fps: 60,
            first_frame_timeout: Duration::ZERO,
            duration: Some(duration),
            max_duration: Duration::from_secs(1),
            heartbeat_interval: Duration::from_millis(1),
            failure_policy: FailurePolicy::Continue,
            stop: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn first_frame_timeout_is_structured_capture_failure() {
        let outcome = run_session(
            vec![pipeline(MockSource {
                frames: VecDeque::from([None]),
                alive: true,
            })],
            options(Duration::from_millis(1)),
            |_| {},
        );
        assert_eq!(outcome.exit_code, 3);
        assert!(outcome.events.iter().any(|event| matches!(
            event,
            Event::Error {
                code: ErrorCode::FirstFrameTimeout,
                ..
            }
        )));
    }

    #[test]
    fn duration_limit_repeats_static_frame_and_saves_intentionally() {
        let outcome = run_session(
            vec![pipeline(MockSource {
                frames: VecDeque::from([Some(frame()), None]),
                alive: true,
            })],
            options(Duration::from_millis(35)),
            |_| {},
        );
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.events.iter().any(|event| matches!(event, Event::Saved { stop_reason: StopReason::DurationLimit, frames, .. } if *frames >= 1)));
    }

    #[test]
    fn target_loss_is_unintended_and_nonzero() {
        let outcome = run_session(
            vec![pipeline(MockSource {
                frames: VecDeque::from([Some(frame())]),
                alive: false,
            })],
            options(Duration::from_millis(50)),
            |_| {},
        );
        assert_ne!(outcome.exit_code, 0);
        assert!(outcome.events.iter().any(|event| matches!(
            event,
            Event::TargetLost {
                stop_reason: StopReason::TargetLost,
                ..
            }
        )));
    }

    #[test]
    fn continue_policy_saves_good_target_and_reports_partial_failure() {
        let good = pipeline(MockSource {
            frames: VecDeque::from([Some(frame()), None]),
            alive: true,
        });
        let mut bad = pipeline(MockSource {
            frames: VecDeque::from([None]),
            alive: true,
        });
        bad.target = "bad.mp4".into();
        bad.output = "bad.mp4".into();
        let outcome = run_session(vec![good, bad], options(Duration::from_millis(35)), |_| {});
        assert_eq!(outcome.exit_code, 6);
        assert!(outcome.events.iter().any(|event| matches!(
            event,
            Event::Saved {
                stop_reason: StopReason::DurationLimit,
                ..
            }
        )));
        assert!(outcome.events.iter().any(|event| matches!(
            event,
            Event::Error {
                code: ErrorCode::PartialFailure,
                ..
            }
        )));
    }

    #[test]
    fn abort_policy_finalizes_other_target_with_aborted_reason() {
        let good = pipeline(MockSource {
            frames: VecDeque::from([Some(frame()), None]),
            alive: true,
        });
        let mut bad = pipeline(MockSource {
            frames: VecDeque::from([None]),
            alive: true,
        });
        bad.target = "bad.mp4".into();
        bad.output = "bad.mp4".into();
        let mut run_options = options(Duration::from_millis(100));
        run_options.failure_policy = FailurePolicy::Abort;
        let outcome = run_session(vec![good, bad], run_options, |_| {});
        assert_eq!(outcome.exit_code, 6);
        assert!(outcome.events.iter().any(|event| matches!(
            event,
            Event::Saved {
                stop_reason: StopReason::AbortedOnFailure,
                ..
            }
        )));
        assert!(outcome.events.iter().any(|event| matches!(
            event,
            Event::Error {
                code: ErrorCode::AbortedOnFailure,
                ..
            }
        )));
    }
}
