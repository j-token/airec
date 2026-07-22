use std::io::{BufRead, BufReader, Write};

use airec_core::{AirecError, ControlRequest, ControlResponse, ErrorCode};
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream, prelude::*};

#[must_use]
pub fn pipe_display_name(session: &str) -> String {
    format!(r"\\.\pipe\airec-{session}")
}

fn pipe_name(session: &str) -> Result<interprocess::local_socket::Name<'static>, AirecError> {
    format!("airec-{session}")
        .to_ns_name::<GenericNamespaced>()
        .map_err(|error| ipc_error(error.to_string()))
}

pub fn serve(
    session: String,
    handler: impl Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
) -> Result<(), AirecError> {
    let listener = ListenerOptions::new()
        .name(pipe_name(&session)?)
        .create_sync()
        .map_err(|error| ipc_error(error.to_string()))?;
    for connection in listener.incoming() {
        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("warning: named-pipe connection failed: {error}");
                continue;
            }
        };
        let mut connection = BufReader::new(connection);
        let mut line = String::new();
        if connection.read_line(&mut line).is_err() {
            continue;
        }
        let response = match serde_json::from_str::<ControlRequest>(&line) {
            Ok(request) => handler(request),
            Err(error) => ControlResponse::Error {
                code: ErrorCode::CaptureInitFailed,
                message: error.to_string(),
            },
        };
        if serde_json::to_writer(connection.get_mut(), &response).is_ok() {
            let _ = connection.get_mut().write_all(b"\n");
            let _ = connection.get_mut().flush();
        }
    }
    Ok(())
}

pub fn request(session: &str, request: &ControlRequest) -> Result<ControlResponse, AirecError> {
    let stream = Stream::connect(pipe_name(session)?).map_err(|error| {
        AirecError::new(
            ErrorCode::NoActiveSession,
            format!("session '{session}' is not reachable: {error}"),
            serde_json::json!({"session": session, "pipe": pipe_display_name(session)}),
        )
    })?;
    let mut stream = BufReader::new(stream);
    serde_json::to_writer(stream.get_mut(), request)
        .map_err(|error| ipc_error(error.to_string()))?;
    stream
        .get_mut()
        .write_all(b"\n")
        .map_err(|error| ipc_error(error.to_string()))?;
    stream
        .get_mut()
        .flush()
        .map_err(|error| ipc_error(error.to_string()))?;
    let mut line = String::new();
    stream
        .read_line(&mut line)
        .map_err(|error| ipc_error(error.to_string()))?;
    serde_json::from_str(&line).map_err(|error| ipc_error(error.to_string()))
}

fn ipc_error(message: String) -> AirecError {
    AirecError::new(
        ErrorCode::CaptureInitFailed,
        message,
        serde_json::json!({"component": "named_pipe"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_matches_prd() {
        assert_eq!(pipe_display_name("a1b2"), r"\\.\pipe\airec-a1b2");
    }
}
