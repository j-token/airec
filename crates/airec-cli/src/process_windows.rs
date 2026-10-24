use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW, DETACHED_PROCESS,
    PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

/// Starts a process that keeps none of the caller's handles, so a parent reading the caller's
/// stdout sees EOF as soon as the caller exits.
pub fn spawn_detached_process(executable: &Path, arguments: &[&OsStr]) -> Result<(), String> {
    let mut executable_wide: Vec<u16> = executable.as_os_str().encode_wide().collect();
    executable_wide.push(0);
    let mut command_line = Vec::new();
    push_windows_argument(&mut command_line, executable.as_os_str());
    for argument in arguments {
        command_line.push(u16::from(b' '));
        push_windows_argument(&mut command_line, argument);
    }
    command_line.push(0);

    let startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>())
            .expect("STARTUPINFOW size fits in u32"),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )
        .map_err(|error| error.to_string())?;
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

fn push_windows_argument(command_line: &mut Vec<u16>, argument: &OsStr) {
    command_line.push(u16::from(b'"'));
    let mut backslashes = 0_usize;
    for unit in argument.encode_wide() {
        if unit == u16::from(b'\\') {
            backslashes += 1;
        } else if unit == u16::from(b'"') {
            command_line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
            command_line.push(unit);
            backslashes = 0;
        } else {
            command_line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            command_line.push(unit);
            backslashes = 0;
        }
    }
    command_line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    command_line.push(u16::from(b'"'));
}

/// Opens the log shared for read, write, and delete so `start` can replay it while the
/// session child still holds it.
pub fn open_diagnostic_log(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .open(path)
}

pub fn redirect_stderr(file: &File) -> Result<(), String> {
    unsafe { SetStdHandle(STD_ERROR_HANDLE, HANDLE(file.as_raw_handle())) }
        .map_err(|error| error.to_string())
}
