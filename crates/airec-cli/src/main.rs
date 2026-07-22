mod args;
mod config;
mod ipc;
mod runtime;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let json_requested = std::env::args_os().any(|argument| argument == "--json");
    let cli = match args::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if error.exit_code() == 0 => {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) if json_requested => {
            let error = airec_core::AirecError::new(
                airec_core::ErrorCode::CaptureInitFailed,
                error.to_string(),
                serde_json::json!({"component": "cli"}),
            );
            runtime::print_error(&error, true);
            return ExitCode::from(error.exit_code() as u8);
        }
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(u8::try_from(code).unwrap_or(1));
        }
    };
    let code = match runtime::execute(cli) {
        Ok(code) => code,
        Err((error, json)) => {
            runtime::print_error(&error, json);
            error.exit_code()
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
