use std::fs;
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde_json::json;
use serde_yaml::{Mapping, Value};

use crate::audit::{AuditEvent, AuditLogger};
use crate::cli::AdminCommand;
use crate::config::{AppConfig, resolve_config_path};
use crate::transport::auth::hash_bearer_token;
use memra_core::storage::db::DbPool;
use memra_core::storage::session_tokens_writer::revoke_tokens_for_session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedKey {
    pub name: String,
    pub actor_id: String,
    pub raw_key: String,
    pub key_hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyListing {
    pub name: String,
    pub actor_id: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

pub fn run_admin(command: AdminCommand) -> anyhow::Result<()> {
    let path = resolve_config_path();
    match command {
        AdminCommand::Revert(args) => {
            let created = create_revert_request(args.reason(), args.force())?;
            println!("Kill switch created: {}", created.display());
            println!("Reason: {}", args.reason());
            println!(
                "R4 no longer installs the archived v6 revert guard; restart MCP clients after reviewing this kill switch."
            );
            Ok(())
        }
        AdminCommand::AddKey(args) => {
            let issued = add_key_at_path(&path, args.name(), args.actor_id())?;
            println!("name: {}", issued.name);
            println!("actor_id: {}", issued.actor_id);
            println!("api_key: {}", issued.raw_key);
            println!("key_hash: {}", issued.key_hash);
            println!("created_at: {}", issued.created_at);
            Ok(())
        }
        AdminCommand::RevokeKey(args) => {
            let revoked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            revoke_key_at_path(&path, args.name(), &revoked_at)?;
            println!("revoked: {}", args.name());
            println!("revoked_at: {revoked_at}");
            Ok(())
        }
        AdminCommand::ListKeys => {
            let keys = list_keys_at_path(&path)?;
            for key in keys {
                println!(
                    "{}\t{}\t{}\t{}",
                    key.name,
                    key.actor_id,
                    key.created_at,
                    key.revoked_at.unwrap_or_else(|| "-".to_string())
                );
            }
            Ok(())
        }
        AdminCommand::ReloadAuth => {
            let config = AppConfig::load_from_path(&path)?;
            println!("auth keys loaded: {}", config.auth.api_keys.len());
            Ok(())
        }
        AdminCommand::SessionRevoke(args) => {
            let revoked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            let db_path = match args.db_path() {
                Some(p) => p.to_path_buf(),
                None => resolve_default_db_path()?,
            };
            let pool = DbPool::open(&db_path)
                .map_err(|e| anyhow::anyhow!("failed to open DB at {}: {e}", db_path.display()))?;
            let actors = pool
                .with_conn(|conn| revoke_tokens_for_session(conn, args.session_id(), &revoked_at))
                .map_err(|e| anyhow::anyhow!("revoke_tokens_for_session failed: {e}"))?;

            // TODO-IMPL-01b-audit fix: emit audit JSONL row so CLI-driven
            // revocations are forensically attributable. Without this, the
            // only signal that a session was revoked is the absence of
            // future `session_token_validated/ok` events for that
            // session_id — which is a non-event and easy to miss in
            // post-hoc audit replay.
            //
            // Self-review P1 fix (PR #236): use result="no_op" when no
            // tokens were actually revoked. Emitting "ok" with
            // revoked_count=0 would make audit replay falsely show "session
            // X revoked" for every CLI invocation that touched a session
            // with no active tokens — masking real revocations and giving
            // an attacker a way to flood audit with success-shaped no-ops.
            let audit = AuditLogger::default();
            let result = if actors.is_empty() { "no_op" } else { "ok" };
            let mut event = AuditEvent::new("session_token_revoked", result)
                .with_session_id(args.session_id().to_string())
                .with_field("revoked_at", revoked_at.as_str())
                .with_field("revoked_count", actors.len() as i64)
                .with_field("source", "ma_admin_cli");
            // Attach the affected actor_ids so audit replay can answer
            // "which actor's tokens were killed?" without joining back to
            // the session_tokens table (which the operator may have
            // pruned via sweep_expired by the time of replay).
            if !actors.is_empty() {
                let actors_json = serde_json::Value::Array(
                    actors
                        .iter()
                        .map(|a| serde_json::Value::String(a.clone()))
                        .collect(),
                );
                event = event.with_field("revoked_actor_ids", actors_json);
            }
            if let Err(error) = audit.append(event) {
                // Audit is best-effort — surface the failure but don't
                // fail the revoke itself (the DB write already succeeded).
                tracing::warn!("session_token_revoked audit append failed (non-fatal): {error}");
            }

            if actors.is_empty() {
                println!("revoked: 0 tokens for session {}", args.session_id());
                println!("(no active write tokens found for this session)");
            } else {
                println!(
                    "revoked: {} token(s) for session {}",
                    actors.len(),
                    args.session_id()
                );
                println!("revoked_at: {revoked_at}");
                for actor in &actors {
                    println!("  actor_id: {actor}");
                }
            }
            Ok(())
        }
    }
}

fn default_ma_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".memra")
}

pub fn create_revert_request(reason: &str, force: bool) -> Result<PathBuf, AdminError> {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    create_revert_request_at_dir(default_ma_dir(), reason, force, &timestamp)
}

pub fn create_revert_request_at_dir(
    ma_dir: impl AsRef<Path>,
    reason: &str,
    force: bool,
    timestamp: &str,
) -> Result<PathBuf, AdminError> {
    let ma_dir = ma_dir.as_ref();
    fs::create_dir_all(ma_dir).map_err(|source| AdminError::Io {
        path: ma_dir.to_path_buf(),
        source,
    })?;
    let path = ma_dir.join("revert-requested");
    if path.exists() && !force {
        return Err(AdminError::RevertRequestExists(path));
    }
    let payload = json!({
        "reason": reason,
        "timestamp": timestamp,
        "triggered_by": "manual",
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&payload)
            .expect("serializing revert request payload cannot fail")
            + "\n",
    )
    .map_err(|source| AdminError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Resolve the default SQLite path for the memra project.
///
/// Mirrors the logic in `main.rs::resolve_db_path` without the tracing calls.
fn resolve_default_db_path() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("MCP_MEMORY_STORAGE_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
        anyhow::bail!(
            "MCP_MEMORY_STORAGE_PATH set but file not found: {}",
            p.display()
        );
    }
    let project_id =
        std::env::var("MCP_MEMORY_PROJECT_ID").unwrap_or_else(|_| "memra".into());
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let default_path = home
        .join(".memra")
        .join("projects")
        .join(&project_id)
        .join(".storage")
        .join("memory_anchor.sqlite3");
    if default_path.exists() {
        return Ok(default_path);
    }
    anyhow::bail!(
        "no SQLite DB found at default path {}; pass --db <PATH>",
        default_path.display()
    )
}

pub fn add_key_at_path(
    path: impl AsRef<Path>,
    name: &str,
    actor_id: &str,
) -> Result<ProvisionedKey, AdminError> {
    let raw_key = generate_key_material()?;
    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    add_key_with_material(path, name, actor_id, &created_at, &raw_key)
}

pub fn add_key_with_material(
    path: impl AsRef<Path>,
    name: &str,
    actor_id: &str,
    created_at: &str,
    raw_key: &str,
) -> Result<ProvisionedKey, AdminError> {
    let path = path.as_ref();
    let mut value = load_config_value(path)?;
    let api_keys = api_keys_sequence_mut(&mut value)?;
    if api_keys.iter().any(|entry| entry_name(entry) == Some(name)) {
        return Err(AdminError::DuplicateName(name.to_string()));
    }

    let key_hash = format!("blake3:{}", hash_bearer_token(raw_key));
    api_keys.push(api_key_value(name, &key_hash, actor_id, created_at, None));
    write_config_value(path, &value)?;

    Ok(ProvisionedKey {
        name: name.to_string(),
        actor_id: actor_id.to_string(),
        raw_key: raw_key.to_string(),
        key_hash,
        created_at: created_at.to_string(),
    })
}

pub fn list_keys_at_path(path: impl AsRef<Path>) -> Result<Vec<KeyListing>, AdminError> {
    let mut value = load_config_value(path.as_ref())?;
    let api_keys = api_keys_sequence_mut(&mut value)?;
    let mut keys = Vec::new();
    for entry in api_keys {
        let Value::Mapping(mapping) = entry else {
            continue;
        };
        let name = string_field(mapping, "name").unwrap_or_default();
        let actor_id = string_field(mapping, "actor_id").unwrap_or_default();
        let created_at = string_field(mapping, "created_at").unwrap_or_default();
        let revoked_at = optional_string_field(mapping, "revoked_at");
        keys.push(KeyListing {
            name,
            actor_id,
            created_at,
            revoked_at,
        });
    }
    Ok(keys)
}

pub fn revoke_key_at_path(
    path: impl AsRef<Path>,
    name: &str,
    revoked_at: &str,
) -> Result<(), AdminError> {
    let path = path.as_ref();
    let mut value = load_config_value(path)?;
    let api_keys = api_keys_sequence_mut(&mut value)?;
    for entry in api_keys {
        if entry_name(entry) == Some(name) {
            let Value::Mapping(mapping) = entry else {
                return Err(AdminError::InvalidConfig(
                    "auth.api_keys entries must be mappings".to_string(),
                ));
            };
            mapping.insert(
                Value::String("revoked_at".to_string()),
                Value::String(revoked_at.to_string()),
            );
            write_config_value(path, &value)?;
            return Ok(());
        }
    }
    Err(AdminError::KeyNotFound(name.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("admin config I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("admin config YAML error: {0}")]
    Yaml(serde_yaml::Error),
    #[error("API key name already exists: {0}")]
    DuplicateName(String),
    #[error("API key not found: {0}")]
    KeyNotFound(String),
    #[error("invalid admin config: {0}")]
    InvalidConfig(String),
    #[error("revert-requested file already exists: {0}; use --force to overwrite")]
    RevertRequestExists(PathBuf),
    #[error("random key generation failed: {0}")]
    Random(String),
}

fn load_config_value(path: &Path) -> Result<Value, AdminError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Value::Mapping(Mapping::new()));
        }
        Err(source) => {
            return Err(AdminError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let value = serde_yaml::from_str::<Value>(&content).map_err(AdminError::Yaml)?;
    if value.is_null() {
        Ok(Value::Mapping(Mapping::new()))
    } else {
        Ok(value)
    }
}

fn write_config_value(path: &Path, value: &Value) -> Result<(), AdminError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| AdminError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("yaml.tmp");
    let content = serde_yaml::to_string(value).map_err(AdminError::Yaml)?;
    fs::write(&tmp, content).map_err(|source| AdminError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| AdminError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn api_keys_sequence_mut(value: &mut Value) -> Result<&mut Vec<Value>, AdminError> {
    let root = mapping_mut(value, "root config")?;
    let auth = child_mapping_mut(root, "auth")?;
    child_sequence_mut(auth, "api_keys")
}

fn mapping_mut<'a>(value: &'a mut Value, label: &str) -> Result<&'a mut Mapping, AdminError> {
    match value {
        Value::Mapping(mapping) => Ok(mapping),
        _ => Err(AdminError::InvalidConfig(format!(
            "{label} must be a mapping"
        ))),
    }
}

fn child_mapping_mut<'a>(
    mapping: &'a mut Mapping,
    key: &str,
) -> Result<&'a mut Mapping, AdminError> {
    let key_value = Value::String(key.to_string());
    if !mapping.contains_key(&key_value) {
        mapping.insert(key_value.clone(), Value::Mapping(Mapping::new()));
    }
    match mapping.get_mut(&key_value) {
        Some(Value::Mapping(child)) => Ok(child),
        _ => Err(AdminError::InvalidConfig(format!(
            "{key} must be a mapping"
        ))),
    }
}

fn child_sequence_mut<'a>(
    mapping: &'a mut Mapping,
    key: &str,
) -> Result<&'a mut Vec<Value>, AdminError> {
    let key_value = Value::String(key.to_string());
    if !mapping.contains_key(&key_value) {
        mapping.insert(key_value.clone(), Value::Sequence(Vec::new()));
    }
    match mapping.get_mut(&key_value) {
        Some(Value::Sequence(sequence)) => Ok(sequence),
        _ => Err(AdminError::InvalidConfig(format!(
            "{key} must be a sequence"
        ))),
    }
}

fn api_key_value(
    name: &str,
    key_hash: &str,
    actor_id: &str,
    created_at: &str,
    revoked_at: Option<&str>,
) -> Value {
    let mut mapping = Mapping::new();
    mapping.insert(
        Value::String("name".to_string()),
        Value::String(name.to_string()),
    );
    mapping.insert(
        Value::String("key_hash".to_string()),
        Value::String(key_hash.to_string()),
    );
    mapping.insert(
        Value::String("actor_id".to_string()),
        Value::String(actor_id.to_string()),
    );
    mapping.insert(
        Value::String("created_at".to_string()),
        Value::String(created_at.to_string()),
    );
    mapping.insert(
        Value::String("revoked_at".to_string()),
        match revoked_at {
            Some(value) => Value::String(value.to_string()),
            None => Value::Null,
        },
    );
    Value::Mapping(mapping)
}

fn entry_name(entry: &Value) -> Option<&str> {
    let Value::Mapping(mapping) = entry else {
        return None;
    };
    mapping
        .get(Value::String("name".to_string()))
        .and_then(Value::as_str)
}

fn string_field(mapping: &Mapping, key: &str) -> Option<String> {
    mapping
        .get(Value::String(key.to_string()))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn optional_string_field(mapping: &Mapping, key: &str) -> Option<String> {
    match mapping.get(Value::String(key.to_string())) {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn generate_key_material() -> Result<String, AdminError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| AdminError::Random(error.to_string()))?;
    Ok(bytes_to_hex(&bytes))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::cli::admin::{
        add_key_with_material, create_revert_request_at_dir, list_keys_at_path, revoke_key_at_path,
    };

    fn temp_config_path(name: &str) -> Result<PathBuf, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ma-admin-test-{now}-{name}"));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(dir.join("config.yaml"))
    }

    #[test]
    fn add_key_writes_hash_but_never_raw_key() -> Result<(), String> {
        let path = temp_config_path("add")?;
        let issued = add_key_with_material(
            &path,
            "local",
            "claude-code",
            "2026-04-13T00:00:00Z",
            "raw-secret",
        )
        .map_err(|error| error.to_string())?;

        let yaml = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        assert_eq!(issued.raw_key, "raw-secret");
        assert!(yaml.contains("name: local"));
        assert!(yaml.contains("actor_id: claude-code"));
        assert!(yaml.contains("key_hash: blake3:"));
        assert!(!yaml.contains("raw-secret"));
        Ok(())
    }

    #[test]
    fn list_keys_omits_key_material() -> Result<(), String> {
        let path = temp_config_path("list")?;
        add_key_with_material(
            &path,
            "local",
            "claude-code",
            "2026-04-13T00:00:00Z",
            "raw-secret",
        )
        .map_err(|error| error.to_string())?;

        let listing = list_keys_at_path(&path).map_err(|error| error.to_string())?;

        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].name, "local");
        assert_eq!(listing[0].actor_id, "claude-code");
        assert!(!format!("{listing:?}").contains("raw-secret"));
        assert!(!format!("{listing:?}").contains("blake3:"));
        Ok(())
    }

    #[test]
    fn revoke_key_marks_revoked_at_without_deleting_entry() -> Result<(), String> {
        let path = temp_config_path("revoke")?;
        add_key_with_material(
            &path,
            "local",
            "claude-code",
            "2026-04-13T00:00:00Z",
            "raw-secret",
        )
        .map_err(|error| error.to_string())?;

        revoke_key_at_path(&path, "local", "2026-04-13T01:00:00Z")
            .map_err(|error| error.to_string())?;
        let listing = list_keys_at_path(&path).map_err(|error| error.to_string())?;

        assert_eq!(listing.len(), 1);
        assert_eq!(
            listing[0].revoked_at.as_deref(),
            Some("2026-04-13T01:00:00Z")
        );
        Ok(())
    }

    #[test]
    fn add_key_rejects_duplicate_names() -> Result<(), String> {
        let path = temp_config_path("duplicate")?;
        add_key_with_material(&path, "local", "alice", "2026-04-13T00:00:00Z", "one")
            .map_err(|error| error.to_string())?;

        let error =
            match add_key_with_material(&path, "local", "bob", "2026-04-13T00:01:00Z", "two") {
                Ok(_) => return Err("expected duplicate rejection".to_string()),
                Err(error) => error,
            };

        assert!(error.to_string().contains("already exists"));
        Ok(())
    }

    #[test]
    fn revert_request_writes_manual_kill_switch_and_respects_force() -> Result<(), String> {
        let path = temp_config_path("revert-request")?;
        let ma_dir = path
            .parent()
            .ok_or_else(|| "missing temp parent".to_string())?
            .join(".memra");

        let request_path = create_revert_request_at_dir(
            &ma_dir,
            "stdout was silently truncating CJK",
            false,
            "2026-05-16T12:00:00Z",
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(request_path, ma_dir.join("revert-requested"));
        let payload = fs::read_to_string(&request_path).map_err(|error| error.to_string())?;
        assert!(payload.contains("\"reason\": \"stdout was silently truncating CJK\""));
        assert!(payload.contains("\"triggered_by\": \"manual\""));
        assert!(payload.contains("\"timestamp\": \"2026-05-16T12:00:00Z\""));

        let duplicate =
            create_revert_request_at_dir(&ma_dir, "new reason", false, "2026-05-16T12:01:00Z")
                .expect_err("expected existing request to require --force");
        assert!(duplicate.to_string().contains("already exists"));

        create_revert_request_at_dir(&ma_dir, "new reason", true, "2026-05-16T12:01:00Z")
            .map_err(|error| error.to_string())?;
        let overwritten = fs::read_to_string(&request_path).map_err(|error| error.to_string())?;
        assert!(overwritten.contains("\"reason\": \"new reason\""));
        Ok(())
    }
}
