//! Port of the settings-manager behaviors the TS config tests pin down,
//! exercised over real files in tempdirs (`config.rs` / `config::settings`).

use pi_coding_agent::config::settings::{
    SettingsManager, SettingsManagerCreateOptions, SettingsScope,
};
use pi_coding_agent::session::compaction::CompactionSettings;
use serde_json::{Value, json};

fn tempdir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("pi-rs-config-{tag}-"))
        .tempdir()
        .unwrap()
}

#[test]
fn project_settings_merge_over_global_settings() {
    let dir = tempdir("merge");
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultProvider": "anthropic",
            "defaultModel": "claude-sonnet-4-5",
            "compaction": {"enabled": true, "reserveTokens": 1000}
        }))
        .unwrap(),
    )
    .unwrap();
    let project_dir = dir.path().join(".pi");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultModel": "gpt-5",
            "compaction": {"reserveTokens": 2000}
        }))
        .unwrap(),
    )
    .unwrap();

    let manager = SettingsManager::create(
        dir.path(),
        dir.path(),
        SettingsManagerCreateOptions::default(),
    );
    assert_eq!(manager.get_default_provider().as_deref(), Some("anthropic"));
    assert_eq!(manager.get_default_model().as_deref(), Some("gpt-5"));
    assert_eq!(
        manager.get_compaction_settings(None).unwrap(),
        CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20_000
        }
    );

    // Scope files are where pi looks for them.
    assert_eq!(
        manager.settings_path_for(SettingsScope::Global),
        Some(dir.path().join("settings.json").as_path())
    );
    assert_eq!(
        manager.settings_path_for(SettingsScope::Project),
        Some(project_dir.join("settings.json").as_path())
    );
}

#[test]
fn untrusted_project_settings_are_skipped_and_writes_refused() {
    let dir = tempdir("untrusted");
    let project_dir = dir.path().join(".pi");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({"defaultProvider": "openai"})).unwrap(),
    )
    .unwrap();

    let mut manager = SettingsManager::create(
        dir.path(),
        dir.path(),
        SettingsManagerCreateOptions {
            project_trusted: Some(false),
        },
    );
    assert!(!manager.is_project_trusted());
    assert_eq!(manager.get_default_provider(), None);

    let error = manager
        .set_project("defaultProvider", json!("anthropic"))
        .unwrap_err();
    assert_eq!(
        error,
        "Project is not trusted; refusing to write project settings"
    );

    // Trusting re-reads the project layer.
    manager.set_project_trusted(true);
    assert_eq!(manager.get_default_provider().as_deref(), Some("openai"));
}

#[test]
fn flushed_global_writes_are_scoped_to_the_modified_field() {
    let dir = tempdir("flush");
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::to_string_pretty(&json!({"theme": "dark", "keepMe": true})).unwrap(),
    )
    .unwrap();

    let mut manager = SettingsManager::create(
        dir.path(),
        dir.path(),
        SettingsManagerCreateOptions::default(),
    );
    manager.set_default_model_and_provider("anthropic", "claude-opus-4-5");
    // One queued write per setter call, as pi's save() chain does.
    assert_eq!(manager.pending_writes(), 2);
    manager.flush();
    assert_eq!(manager.pending_writes(), 0);

    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(stored["defaultProvider"], json!("anthropic"));
    assert_eq!(stored["defaultModel"], json!("claude-opus-4-5"));
    // Unmarked fields written by "another process" survive.
    assert_eq!(stored["keepMe"], json!(true));
    // pi's two-space indent without a trailing newline.
    let raw = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
    assert!(!raw.ends_with('\n'));
}

#[test]
fn project_writes_require_trust_and_flush() {
    let dir = tempdir("project-write");
    let mut manager = SettingsManager::create(
        dir.path(),
        dir.path(),
        SettingsManagerCreateOptions::default(),
    );
    manager
        .set_project("defaultModel", json!("claude-opus-4-5"))
        .unwrap();
    manager.flush();

    let stored: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".pi").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["defaultModel"], json!("claude-opus-4-5"));
}

#[test]
fn a_malformed_global_file_is_never_overwritten() {
    let dir = tempdir("corrupt");
    std::fs::write(dir.path().join("settings.json"), "{not json").unwrap();
    let mut manager = SettingsManager::create(
        dir.path(),
        dir.path(),
        SettingsManagerCreateOptions::default(),
    );
    assert_eq!(manager.get_settings().len(), 0);
    let errors = manager.drain_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].scope, SettingsScope::Global);
    assert_eq!(
        errors[0].path.as_deref(),
        Some(dir.path().join("settings.json").as_path())
    );

    // `save()` bails while the load error stands: the file stays malformed.
    manager.set("theme", json!("dark"));
    manager.flush();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("settings.json")).unwrap(),
        "{not json"
    );
}

#[test]
fn overrides_apply_at_runtime_but_never_persist() {
    let dir = tempdir("overrides");
    let mut manager = SettingsManager::create(
        dir.path(),
        dir.path(),
        SettingsManagerCreateOptions::default(),
    );
    manager.apply_overrides(
        &json!({"defaultProvider": "openai"})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(manager.get_default_provider().as_deref(), Some("openai"));

    manager.set("theme", json!("dark"));
    manager.flush();
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(stored["theme"], json!("dark"));
    assert!(
        stored.get("defaultProvider").is_none(),
        "overrides are runtime-only"
    );
}

#[test]
fn legacy_setting_keys_migrate_on_load() {
    let dir = tempdir("migrate");
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::to_string_pretty(&json!({"queueMode": "all", "websockets": true})).unwrap(),
    )
    .unwrap();
    let manager = SettingsManager::create(
        dir.path(),
        dir.path(),
        SettingsManagerCreateOptions::default(),
    );
    assert_eq!(manager.get("steeringMode"), Some(&json!("all")));
    assert_eq!(manager.get("transport"), Some(&json!("websocket")));
    assert!(manager.get("queueMode").is_none());
    assert!(manager.get("websockets").is_none());
}

#[test]
fn in_memory_seeds_and_persists_through_the_same_path() {
    let mut manager = SettingsManager::in_memory(
        &json!({"theme": "dark"}).as_object().unwrap().clone(),
        SettingsManagerCreateOptions::default(),
    );
    assert_eq!(manager.get("theme"), Some(&json!("dark")));
    manager.set("defaultProvider", json!("anthropic"));
    manager.flush();
    assert_eq!(manager.get_default_provider().as_deref(), Some("anthropic"));
}
