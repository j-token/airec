mod args;
mod ipc;
mod runtime;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = args::Cli::parse();
    let code = match runtime::execute(cli) {
        Ok(code) => code,
        Err((error, json)) => {
            runtime::print_error(&error, json);
            error.exit_code()
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
