//! Port of the file-backed `auth-storage.test.ts` behaviors that map onto a
//! synchronous store, exercised over tempdirs (never the machine's real
//! auth files).

use pi_coding_agent::auth::{AuthStorage, AuthStorageData, Credential, CredentialInfo};
use serde_json::{Value, json};
use std::path::PathBuf;

fn tempdir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("pi-rs-auth-{tag}-"))
        .tempdir()
        .unwrap()
}

fn credential(key: &str) -> Credential {
    Credential::ApiKey {
        key: Some(key.to_string()),
        env: None,
        unknown: serde_json::Map::new(),
    }
}

#[test]
fn reads_and_resolves_stored_api_key_credentials() {
    let dir = tempdir("resolve");
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "anthropic": {"type": "api_key", "key": "$PI_TEST_AUTH_STORAGE_KEY"}
        }))
        .unwrap(),
    )
    .unwrap();
    let storage = AuthStorage::create(&path);
    // The referenced variable is unset: the key resolves to nothing (TS:
    // `{ ...credential, key: undefined }`).
    assert_eq!(
        storage.read("anthropic").unwrap(),
        Some(Credential::ApiKey {
            key: None,
            env: None,
            unknown: serde_json::Map::new()
        })
    );
}

#[test]
fn credential_scoped_env_takes_precedence_and_remains_inspectable() {
    let dir = tempdir("scoped-env");
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "anthropic": {
                "type": "api_key",
                "key": "$SCOPED_KEY",
                "env": {"SCOPED_KEY": "scoped-value", "REGION": "test-region"}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let storage = AuthStorage::create(&path);
    let credential = storage.read("anthropic").unwrap().unwrap();
    match &credential {
        Credential::ApiKey { key, env, .. } => {
            assert_eq!(key.as_deref(), Some("scoped-value"));
            let env = env.as_ref().unwrap();
            assert_eq!(env.get("SCOPED_KEY"), Some(&json!("scoped-value")));
            assert_eq!(env.get("REGION"), Some(&json!("test-region")));
        }
        Credential::Oauth { .. } => panic!("unexpected credential {credential:?}"),
    }
}

#[test]
fn modify_persists_a_credential_while_preserving_unrelated_external_edits() {
    let dir = tempdir("external");
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({"anthropic": {"type": "api_key", "key": "old"}})).unwrap(),
    )
    .unwrap();
    let storage = AuthStorage::create(&path);
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "anthropic": {"type": "api_key", "key": "old"},
            "openai": {"type": "api_key", "key": "external"}
        }))
        .unwrap(),
    )
    .unwrap();

    storage
        .modify("anthropic", |_| Ok(Some(credential("new"))))
        .unwrap();
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        stored,
        json!({
            "anthropic": {"type": "api_key", "key": "new"},
            "openai": {"type": "api_key", "key": "external"}
        })
    );
}

#[test]
fn serializes_modifications_across_instances() {
    let dir = tempdir("serialize");
    let path = dir.path().join("auth.json");
    std::fs::write(&path, "{}").unwrap();
    let first = AuthStorage::create(&path);
    let second = AuthStorage::create(&path);

    first
        .modify("anthropic", |_| Ok(Some(credential("anthropic-key"))))
        .unwrap();
    second
        .modify("openai", |_| Ok(Some(credential("openai-key"))))
        .unwrap();

    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        stored,
        json!({
            "anthropic": {"type": "api_key", "key": "anthropic-key"},
            "openai": {"type": "api_key", "key": "openai-key"}
        })
    );
}

#[test]
fn delete_removes_one_credential_while_preserving_others() {
    let dir = tempdir("delete");
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "anthropic": {"type": "api_key", "key": "anthropic-key"},
            "openai": {"type": "api_key", "key": "openai-key"},
            "google": {"type": "oauth", "access": "a", "refresh": "r", "expires": 0}
        }))
        .unwrap(),
    )
    .unwrap();
    let storage = AuthStorage::create(&path);
    storage.delete("anthropic").unwrap();
    assert_eq!(
        storage.list().unwrap(),
        vec![
            CredentialInfo {
                provider_id: "openai".to_string(),
                credential_type: "api_key".to_string()
            },
            CredentialInfo {
                provider_id: "google".to_string(),
                credential_type: "oauth".to_string()
            },
        ]
    );
    assert_eq!(storage.read("anthropic").unwrap(), None);
    assert_eq!(
        storage.read("openai").unwrap(),
        Some(credential("openai-key"))
    );
    match storage.read("google").unwrap() {
        Some(Credential::Oauth {
            access,
            refresh,
            expires,
            ..
        }) => {
            assert_eq!(access, "a");
            assert_eq!(refresh, "r");
            assert_eq!(expires, 0);
        }
        other => panic!("unexpected credential {other:?}"),
    }
}

#[test]
fn does_not_overwrite_malformed_auth_files() {
    let dir = tempdir("malformed");
    let path = dir.path().join("auth.json");
    std::fs::write(&path, "{invalid-json").unwrap();
    let storage = AuthStorage::create(&path);
    assert!(
        storage
            .modify("openai", |_| Ok(Some(credential("new"))))
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{invalid-json");
}

#[test]
fn creates_new_auth_files_with_owner_only_permissions() {
    let dir = tempdir("perms");
    let path = dir.path().join("nested").join("auth.json");
    let storage = AuthStorage::create(&path);
    assert_eq!(storage.read("missing").unwrap(), None);
    assert!(path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn strip_bom_tolerates_a_byte_order_mark() {
    // pi strips a leading BOM on read (`utils/text.ts`).
    let dir = tempdir("bom");
    let path = dir.path().join("auth.json");
    std::fs::write(&path, "\u{feff}{}").unwrap();
    let storage = AuthStorage::create(&path);
    assert_eq!(storage.read("anything").unwrap(), None);
    // A write after a BOM read replaces the whole file cleanly.
    storage
        .modify("anthropic", |_| Ok(Some(credential("k"))))
        .unwrap();
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(stored["anthropic"]["key"], json!("k"));
}

#[test]
fn unknown_credential_fields_survive_a_modify() {
    let dir = tempdir("unknown");
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "anthropic": {"type": "api_key", "key": "old", "extra": {"a": 1}}
        }))
        .unwrap(),
    )
    .unwrap();
    let storage = AuthStorage::create(&path);
    // The callback gets the current credential (with its extra fields) and
    // can carry it forward.
    storage
        .modify("anthropic", |current| {
            let Some(Credential::ApiKey {
                key: _,
                env,
                unknown,
            }) = current
            else {
                return Ok(None);
            };
            Ok(Some(Credential::ApiKey {
                key: Some("new".to_string()),
                env,
                unknown,
            }))
        })
        .unwrap();
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        stored["anthropic"],
        json!({"type": "api_key", "key": "new", "extra": {"a": 1}})
    );
}

#[test]
fn in_memory_seeds_the_same_json_round_trip() {
    let mut data: AuthStorageData = serde_json::Map::new();
    data.insert(
        "anthropic".to_string(),
        serde_json::to_value(credential("initial")).unwrap(),
    );
    let storage = AuthStorage::in_memory(&data);
    assert_eq!(
        storage.read("anthropic").unwrap(),
        Some(credential("initial"))
    );
    storage
        .modify("anthropic", |_| Ok(Some(credential("updated"))))
        .unwrap();
    assert_eq!(
        storage.read("anthropic").unwrap(),
        Some(credential("updated"))
    );
    storage.delete("anthropic").unwrap();
    assert_eq!(storage.list().unwrap(), Vec::<CredentialInfo>::new());
}

#[test]
fn read_stored_credential_returns_the_raw_stored_value() {
    let dir = tempdir("one-off");
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "anthropic": {"type": "api_key", "key": "$UNRESOLVED_VAR"},
            "openai": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1}
        }))
        .unwrap(),
    )
    .unwrap();
    let anthropic = pi_coding_agent::auth::read_stored_credential("anthropic", &path).unwrap();
    match &anthropic {
        Credential::ApiKey { key, .. } => assert_eq!(key.as_deref(), Some("$UNRESOLVED_VAR")),
        Credential::Oauth { .. } => panic!("unexpected credential {anthropic:?}"),
    }
    let openai = pi_coding_agent::auth::read_stored_credential("openai", &path).unwrap();
    assert_eq!(openai.type_name(), "oauth");
    assert_eq!(
        pi_coding_agent::auth::read_stored_credential("google", &path),
        None
    );
}

#[test]
fn default_path_sits_under_the_agent_dir() {
    let default = AuthStorage::default_path();
    let agent_dir = pi_coding_agent::config::get_agent_dir();
    let expected: PathBuf = agent_dir.join("auth.json");
    assert_eq!(default, expected);
}
