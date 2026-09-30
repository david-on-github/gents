use super::*;
use crate::storage_backend::incompatible_store;

const ACCESS_TOKEN: &str = "at-rest-access-token-5f0c1d";
const REFRESH_TOKEN: &str = "at-rest-refresh-token-9a7e42";

async fn write_credential(node: &defra_node::EmbeddedNode) {
    crate::schema::ensure_runtime_schemas(node)
        .await
        .expect("schemas");
    let response = node
        .execute(&format!(
            r#"mutation {{ create_OAuthCredential(input: {{ credential_id: "claude-oauth:did:key:zA", agent_did: "did:key:zA", provider: "claude-oauth", access_token: "{ACCESS_TOKEN}", refresh_token: "{REFRESH_TOKEN}", is_fedramp: false, enabled: true }}) {{ _docID }} }}"#
        ))
        .await;
    assert!(!response.has_errors(), "{:?}", response.errors);
}

async fn read_refresh_token(node: &defra_node::EmbeddedNode) -> Option<String> {
    let response = node.execute("{ OAuthCredential { refresh_token } }").await;
    assert!(!response.has_errors(), "{:?}", response.errors);
    response.data.as_ref()?["OAuthCredential"][0]["refresh_token"]
        .as_str()
        .map(str::to_owned)
}

fn files_containing(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read store dir") {
            let path = entry.expect("store entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if std::fs::read(&path)
                .expect("read store file")
                .windows(needle.len())
                .any(|window| window == needle)
            {
                found.push(path);
            }
        }
    }
    found
}

fn file_key(temp: &tempfile::TempDir) -> (StoreEncryption, StoreKey, PathBuf) {
    let key_file = home_key_file(temp.path());
    let (record, key) =
        StoreEncryption::create(StoreKeyCustodyChoice::File, &key_file).expect("file store key");
    (record, key, key_file)
}

/// The written credential's tokens reach disk only as ciphertext, while the
/// same writes to an unencrypted store leave them readable, so the scan
/// proves the encryption rather than the storage format.
#[tokio::test]
async fn an_encrypted_store_keeps_credential_tokens_off_disk_and_reopens_with_its_key() {
    let temp = tempfile::tempdir().unwrap();
    let (record, key, key_file) = file_key(&temp);
    assert_eq!(record.custody, StoreKeyCustody::File);
    let data = temp.path().join("data");

    let node = persistent_builder(&data, &key)
        .unwrap()
        .build()
        .await
        .unwrap();
    write_credential(&node).await;
    node.shutdown().await;
    drop(node);
    for token in [ACCESS_TOKEN, REFRESH_TOKEN] {
        let leaked = files_containing(&data, token.as_bytes());
        assert!(leaked.is_empty(), "{token} is on disk in {leaked:?}");
    }

    let key = record.load(&key_file, &data).expect("recorded key");
    let reopened = persistent_builder(&data, &key)
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(
        read_refresh_token(&reopened).await.as_deref(),
        Some(REFRESH_TOKEN)
    );
    reopened.shutdown().await;

    let plain = temp.path().join("plain");
    let control = defra_node::EmbeddedNode::builder()
        .data_path(&plain)
        .with_storage_backend(defra_node::StorageBackend::Regolith)
        .build()
        .await
        .unwrap();
    write_credential(&control).await;
    control.shutdown().await;
    drop(control);
    assert!(
        !files_containing(&plain, REFRESH_TOKEN.as_bytes()).is_empty(),
        "the control store must expose the token, or the scan proves nothing"
    );
}

#[test]
fn a_removed_key_file_is_a_missing_key_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let (record, _key, key_file) = file_key(&temp);
    std::fs::remove_file(&key_file).unwrap();
    let data = temp.path().join("data");

    let error = record.load(&key_file, &data).unwrap_err();
    let store = incompatible_store(&error, &data).expect("typed refusal");
    assert_eq!(store.kind, IncompatibleStoreKind::MissingStoreKey);
    assert!(store.kind.is_older());
    assert!(
        error.to_string().contains("Re-initialize the home"),
        "{error}"
    );
}

#[test]
fn a_new_file_key_never_replaces_an_existing_one() {
    let temp = tempfile::tempdir().unwrap();
    let (_record, _key, key_file) = file_key(&temp);
    let original = std::fs::read(&key_file).unwrap();
    assert!(StoreEncryption::create(StoreKeyCustodyChoice::File, &key_file).is_err());
    assert_eq!(std::fs::read(&key_file).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

/// Only `errSecItemNotFound` proves the key is gone. A locked keychain or a
/// denied prompt is a plain error that no host treats as a store refusal,
/// so no reset or deletion is offered for it.
#[test]
fn only_a_definitely_absent_keychain_item_is_a_missing_key() {
    let data = Path::new("/tmp/gents-home/data");
    let missing = keychain_failure(crate::identity::ERR_SEC_ITEM_NOT_FOUND, data);
    assert_eq!(
        incompatible_store(&missing, data).map(|store| store.kind),
        Some(IncompatibleStoreKind::MissingStoreKey)
    );

    for code in [-25308, -25293, -128, -34018] {
        let unavailable = keychain_failure(code, data);
        assert!(
            incompatible_store(&unavailable, data).is_none(),
            "Keychain status {code} must not refuse the store"
        );
        let typed = unavailable
            .downcast_ref::<StoreKeyUnavailable>()
            .expect("typed unavailable key");
        assert_eq!(typed.code, code);
        assert!(unavailable.to_string().contains("the store is unchanged"));
    }
}

#[test]
fn a_home_initialized_without_store_encryption_is_refused_as_unencrypted() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::home::init_config_path(&home),
        serde_json::to_vec(&serde_json::json!({
            "home": home, "agent_name": "a", "agent_did": "did:key:zA",
            "key_path": null, "tool_ceiling": "readonly", "tool_root": null
        }))
        .unwrap(),
    )
    .unwrap();
    let data = crate::home::default_data_dir(&home);

    let error = open_home_store_key(&home, &data).unwrap_err();
    let store = incompatible_store(&error, &data).expect("typed refusal");
    assert_eq!(store.kind, IncompatibleStoreKind::UnencryptedStore);
    assert!(store.kind.is_older());
    assert!(
        error.to_string().contains("--dangerously-overwrite"),
        "{error}"
    );
}

#[tokio::test]
async fn a_store_with_data_but_no_recorded_key_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    reject_unrecorded_store(&data).expect("an absent store has nothing to refuse");

    let legacy = defra_node::EmbeddedNode::builder()
        .data_path(&data)
        .with_storage_backend(defra_node::StorageBackend::Regolith)
        .build()
        .await
        .unwrap();
    legacy.shutdown().await;
    drop(legacy);

    let error = reject_unrecorded_store(&data).unwrap_err();
    assert_eq!(
        incompatible_store(&error, &data).map(|store| store.kind),
        Some(IncompatibleStoreKind::UnencryptedStore)
    );
}

#[test]
fn an_unknown_record_version_is_not_offered_for_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let (mut record, _key, key_file) = file_key(&temp);
    record.version = STORE_ENCRYPTION_VERSION + 1;
    let data = temp.path().join("data");
    let error = record.load(&key_file, &data).unwrap_err();
    let store = incompatible_store(&error, &data).expect("typed refusal");
    assert_eq!(store.kind, IncompatibleStoreKind::ForeignVersion);
    assert!(!store.kind.is_older());
}

#[test]
fn records_round_trip_through_their_persisted_shape() {
    let keychain = StoreEncryption {
        version: STORE_ENCRYPTION_VERSION,
        custody: StoreKeyCustody::MacosKeychain {
            keychain_label: "store-abc".into(),
        },
    };
    let value = serde_json::to_value(&keychain).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"version": 1, "custody": "macos-keychain", "keychain_label": "store-abc"})
    );
    assert_eq!(
        serde_json::from_value::<StoreEncryption>(value).unwrap(),
        keychain
    );
    let file = serde_json::json!({"version": 1, "custody": "file"});
    assert_eq!(
        serde_json::from_value::<StoreEncryption>(file)
            .unwrap()
            .custody,
        StoreKeyCustody::File
    );
}

#[test]
fn a_store_key_is_never_printed() {
    let temp = tempfile::tempdir().unwrap();
    let (_record, key, key_file) = file_key(&temp);
    let bytes = std::fs::read(key_file).unwrap();
    let debug = format!("{key:?}");
    assert_eq!(debug, "StoreKey([redacted])");
    assert!(!debug.contains(&format!("{:?}", bytes)));
}

#[test]
fn an_unrecorded_identity_file_is_preserved_for_every_custody_choice() {
    let temp = tempfile::tempdir().unwrap();
    let key_file = home_key_file(temp.path());
    crate::identity::load_or_create_file_identity(&key_file).unwrap();
    let original = std::fs::read(&key_file).unwrap();
    for choice in [StoreKeyCustodyChoice::File, StoreKeyCustodyChoice::Keychain] {
        let error = open_or_create_store_key(None, choice, &key_file, &temp.path().join("data"))
            .unwrap_err();
        assert!(error.to_string().contains("refusing to replace"), "{error}");
        assert_eq!(std::fs::read(&key_file).unwrap(), original);
        crate::identity::load_file_identity(&key_file).unwrap();
    }
}
