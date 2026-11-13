#![cfg(feature = "desktop")]

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
use voiceisolator::{
    PROFILE_SAMPLES,
    engine::{Config, ENGINE_PROTOCOL, default_config},
    preferences::{self, Profile},
};

#[test]
fn child_reports_missing_profile_before_opening_devices() -> anyhow::Result<()> {
    let app_data = tempfile::tempdir()?;
    let config = default_config("unused microphone".to_owned(), "unused cable".to_owned());
    let updates = run_child(app_data.path(), &config)?;
    assert!(
        updates
            .iter()
            .any(|update| update["Fault"] == "Record your voice before starting isolation.")
    );
    assert!(
        updates
            .iter()
            .all(|update| update["Status"]["live"].as_bool() != Some(true))
    );
    Ok(())
}

#[test]
fn child_uses_the_selected_profile() -> anyhow::Result<()> {
    let app_data = tempfile::tempdir()?;
    let legacy = app_data.path().join("VoiceIsolator/voice-profile.dat");
    let named = preferences::named_profile_path(&legacy, Some("Test voice"))?;
    let profile = |microphone: &str| Profile {
        microphone: microphone.into(),
        samples: vec![0.1; PROFILE_SAMPLES],
    };
    preferences::save_profile(&legacy, &profile("legacy mic"))?;
    preferences::create_profile(&named, &profile("test mic"))?;
    let mut config = default_config("legacy mic".into(), "unused cable".into());
    config.voice_profile = Some("Test voice".into());
    let updates = run_child(app_data.path(), &config)?;
    assert!(
        updates
            .iter()
            .any(|u| u["Fault"] == "Record your voice again for the selected microphone."),
        "{updates:?}"
    );
    preferences::delete_profile(&named)?;
    let updates = run_child(app_data.path(), &config)?;
    assert!(
        updates
            .iter()
            .any(|u| u["Fault"] == "Record your voice before starting isolation."),
        "{updates:?}"
    );
    Ok(())
}

fn run_child(app_data: &Path, config: &Config) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_voiceisolator_engine"))
        .env("LOCALAPPDATA", app_data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = serde_json::json!({"Start": {"protocol": ENGINE_PROTOCOL, "config": config}});
    writeln!(child.stdin.take().unwrap(), "{start}")?;
    let result = child.wait_with_output()?;
    assert!(!result.status.success());
    let messages = String::from_utf8(result.stdout)?;
    Ok(messages
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?)
}
