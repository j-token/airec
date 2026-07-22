use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn structured_no_session_error_has_prd_exit_and_one_json_line() {
    let output = Command::new(env!("CARGO_BIN_EXE_airec"))
        .args([
            "stop",
            "--session",
            "definitely-not-an-airec-session",
            "--json",
        ])
        .output()
        .expect("run airec");
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["event"], "error");
    assert_eq!(value["code"], "NO_ACTIVE_SESSION");
    assert_eq!(value["session"], "none");
    assert!(value["target"].is_null());
}

#[test]
fn help_exposes_every_v01_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_airec"))
        .arg("--help")
        .output()
        .expect("run airec help");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for command in ["list", "record", "start", "status", "stop", "doctor"] {
        assert!(text.contains(command), "missing {command} from help");
    }
    assert!(!text.contains("_session"));
}

#[test]
fn clap_validation_error_is_structured_when_json_is_requested() {
    let output = Command::new(env!("CARGO_BIN_EXE_airec"))
        .args(["record", "--fps", "61", "--json"])
        .output()
        .expect("run airec");
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["event"], "error");
    assert_eq!(value["code"], "CAPTURE_INIT_FAILED");
    assert_eq!(value["data"]["component"], "cli");
}

#[test]
fn detached_start_pipe_reaches_eof_before_session_child_exits() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let ready = std::env::temp_dir().join(format!(
        "airec-detach-pipe-{}-{nonce}.ready",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&ready);

    let started = Instant::now();
    let mut process = Command::new(env!("CARGO_BIN_EXE_airec"))
        .args(["start", "--json"])
        .env("AIREC_TEST_DETACHED_PIPE_PROBE", &ready)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn piped airec start");
    let mut stdout = process.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });

    let status = process.wait().expect("wait for airec start parent");
    assert!(status.success());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "start parent did not return promptly"
    );
    let bytes = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("stdout reader did not observe EOF while detached child was still alive")
        .expect("read start stdout");
    let stdout = String::from_utf8(bytes).expect("start stdout is UTF-8");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("started JSON");
    assert_eq!(value["event"], "started");
    assert!(
        ready.exists(),
        "detached test child never signaled readiness"
    );
    let _ = std::fs::remove_file(ready);
}
