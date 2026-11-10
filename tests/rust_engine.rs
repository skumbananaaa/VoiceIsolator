#![cfg(feature = "desktop")]

use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn child_reports_missing_profile_before_opening_devices() -> anyhow::Result<()> {
    let app_data = tempfile::tempdir()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_voiceisolator_engine"))
        .env("LOCALAPPDATA", app_data.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let config = voiceisolator::engine::default_config(
        "unused microphone".to_owned(),
        "unused cable".to_owned(),
    );
    let start = serde_json::json!({"Start": {"protocol": 1, "config": config}});
    writeln!(child.stdin.take().unwrap(), "{start}")?;
    let result = child.wait_with_output()?;
    assert!(!result.status.success());
    let messages = String::from_utf8(result.stdout)?;
    let updates = messages
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        updates.iter().any(|update| update["Fault"].as_str()
            == Some("Record your voice before starting isolation.")),
        "Expected a closed route with an enrollment error, got {messages:?}"
    );
    assert!(
        updates
            .iter()
            .all(|update| update["Status"]["live"].as_bool() != Some(true))
    );
    Ok(())
}
