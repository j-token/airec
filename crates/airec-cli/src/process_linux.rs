use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Starts the process in its own session with no controlling terminal and with stdio on
/// `/dev/null`, so a parent reading the caller's stdout sees EOF as soon as the caller exits and
/// closing the terminal does not deliver SIGHUP to the recording.
pub fn spawn_detached_process(executable: &Path, arguments: &[&OsStr]) -> Result<(), String> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The child outlives this process and is reparented to init, which reaps it.
    command.spawn().map(drop).map_err(|error| error.to_string())
}

pub fn open_diagnostic_log(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
}

pub fn redirect_stderr(file: &File) -> Result<(), String> {
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
