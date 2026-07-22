use std::process::Command;

#[test]
fn structured_no_session_error_has_prd_exit_and_one_json_line() {
    let output = Command::new(env!("CARGO_BIN_EXE_airec"))
        .args(["stop", "--session", "definitely-not-an-airec-session", "--json"])
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
