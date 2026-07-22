use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "airec",
    version,
    about = "Windows CLI screen recorder for AI work evidence"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Enumerate capturable monitors or windows.
    List(ListArgs),
    /// Record in the foreground until duration or Ctrl+C.
    Record(RecordArgs),
    /// Start a detached recording session.
    Start(StartArgs),
    /// List active recording sessions.
    Status(JsonArgs),
    /// Finalize and stop a detached recording session.
    Stop(StopArgs),
    /// Diagnose Windows Graphics Capture and encoder configuration.
    Doctor(JsonArgs),
    #[command(name = "_session", hide = true)]
    Session(SessionArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(subcommand)]
    pub kind: ListKind,
}

#[derive(Debug, Subcommand)]
pub enum ListKind {
    Monitors(JsonArgs),
    Windows(JsonArgs),
}

#[derive(Clone, Debug, Args)]
pub struct JsonArgs {
    /// Emit machine-readable JSON/JSONL to stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RecordArgs {
    #[command(flatten)]
    pub targets: TargetArgs,
    #[command(flatten)]
    pub recording: RecordingArgs,
    /// Foreground recording duration, for example 30s or 2m; omit to use Ctrl+C.
    #[arg(long)]
    pub duration: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    #[command(flatten)]
    pub targets: TargetArgs,
    #[command(flatten)]
    pub recording: RecordingArgs,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Debug, Default, Args)]
pub struct TargetArgs {
    /// One-based monitor index, repeatable, or `all`.
    #[arg(long)]
    pub monitor: Vec<String>,
    /// Window title substring; repeat for multiple windows.
    #[arg(long)]
    pub window: Vec<String>,
    /// Decimal or 0x-prefixed HWND; repeat for multiple windows.
    #[arg(long = "window-handle", value_parser = parse_hwnd)]
    pub window_handle: Vec<isize>,
    /// Process executable name; all matching top-level windows are recorded.
    #[arg(long)]
    pub process: Vec<String>,
}

#[derive(Clone, Debug, Args)]
pub struct RecordingArgs {
    /// Output file for a single target.
    #[arg(long, conflicts_with = "out_dir")]
    pub out: Option<PathBuf>,
    /// Output directory for multiple targets.
    #[arg(long, conflicts_with = "out")]
    pub out_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=60))]
    pub fps: u32,
    #[arg(long, value_enum, default_value_t = QualityArg::Medium)]
    pub quality: QualityArg,
    #[arg(long)]
    pub no_cursor: bool,
    #[arg(long)]
    pub no_effects: bool,
    #[arg(long, default_value = "30m")]
    pub max_duration: String,
    #[arg(long, value_enum, default_value_t = FailureArg::Continue)]
    pub on_failure: FailureArg,
    #[arg(long)]
    pub event_log: Option<PathBuf>,
    #[arg(long)]
    pub verbose: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum QualityArg {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum FailureArg {
    Continue,
    Abort,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionArgs {
    #[arg(long)]
    pub config: PathBuf,
}

fn parse_hwnd(value: &str) -> Result<isize, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        isize::from_str_radix(hex, 16).map_err(|error| error.to_string())
    } else {
        value
            .parse()
            .map_err(|error: std::num::ParseIntError| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[test]
    fn clap_contract_is_internally_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn repeated_targets_and_hex_hwnd_parse() {
        let cli = Cli::try_parse_from([
            "airec",
            "start",
            "--monitor",
            "1",
            "--monitor",
            "2",
            "--window",
            "App",
            "--window-handle",
            "0x1a2b",
            "--out-dir",
            "rec",
            "--json",
        ])
        .unwrap();
        let Command::Start(args) = cli.command else {
            panic!("expected start")
        };
        assert_eq!(args.targets.monitor, ["1", "2"]);
        assert_eq!(args.targets.window_handle, [0x1a2b]);
        assert!(args.json);
    }

    #[test]
    fn fps_above_sixty_is_rejected() {
        assert!(
            Cli::try_parse_from(["airec", "record", "--duration", "1s", "--fps", "61"]).is_err()
        );
    }

    #[test]
    fn record_duration_is_optional_for_ctrl_c_mode() {
        let cli = Cli::try_parse_from(["airec", "record", "--no-effects"]).unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record")
        };
        assert!(args.duration.is_none());
    }
}
