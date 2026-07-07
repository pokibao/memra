use std::env;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

pub const CONFIG_PATH_ENV: &str = "MCP_MEMORY_CONFIG_PATH";
pub const DEFAULT_HTTP_BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
pub const DEFAULT_HTTP_PORT: u16 = 7331;

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub auth: AuthConfig,
    pub server: ServerConfig,
}

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from_path(resolve_config_path())
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        let value: serde_yaml::Value =
            serde_yaml::from_str(&content).map_err(|source| ConfigError::InvalidYaml {
                path: path.to_path_buf(),
                source,
            })?;

        if value.is_null() {
            return Ok(Self::default());
        }

        serde_yaml::from_value(value).map_err(|source| ConfigError::InvalidYaml {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub api_keys: Vec<ApiKeyConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiKeyConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(alias = "hash")]
    pub key_hash: String,
    pub actor_id: String,
    pub created_at: String,
    #[serde(default)]
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub tls: TlsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_HTTP_BIND,
            port: DEFAULT_HTTP_PORT,
            tls: TlsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid YAML in config {path}: {source}")]
    InvalidYaml {
        path: PathBuf,
        source: serde_yaml::Error,
    },
}

pub fn resolve_config_path() -> PathBuf {
    let explicit = env::var_os(CONFIG_PATH_ENV).map(PathBuf::from);
    resolve_config_path_from(explicit, &home_dir())
}

pub fn resolve_config_path_from(explicit: Option<PathBuf>, home: &Path) -> PathBuf {
    match explicit {
        Some(path) => path,
        None => home.join(".memra").join("config.yaml"),
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{AppConfig, ConfigError, resolve_config_path_from};

    fn temp_config_path(name: &str) -> Result<PathBuf, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("memra-server-config-test-{now}-{name}"));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(dir.join("config.yaml"))
    }

    #[test]
    fn missing_config_uses_safe_defaults() -> Result<(), String> {
        let path = temp_config_path("missing")?;
        let config = AppConfig::load_from_path(&path).map_err(|error| error.to_string())?;

        assert!(config.auth.api_keys.is_empty());
        assert_eq!(config.server.bind.to_string(), "127.0.0.1");
        assert_eq!(config.server.port, 7331);
        assert!(config.server.tls.cert.is_none());
        assert!(config.server.tls.key.is_none());
        Ok(())
    }

    #[test]
    fn loads_auth_and_server_sections_from_yaml() -> Result<(), String> {
        let path = temp_config_path("yaml")?;
        fs::write(
            &path,
            r#"
auth:
  api_keys:
    - name: claude-code-local
      key_hash: "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      actor_id: "claude-code"
      created_at: "2026-04-12T00:00:00Z"
      revoked_at: null
    - hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
      actor_id: "cursor"
      created_at: "2026-04-12T01:00:00Z"
server:
  bind: "0.0.0.0"
  port: 8443
  tls:
    cert: "/tmp/cert.pem"
    key: "/tmp/key.pem"
"#,
        )
        .map_err(|error| error.to_string())?;

        let config = AppConfig::load_from_path(&path).map_err(|error| error.to_string())?;

        assert_eq!(config.auth.api_keys.len(), 2);
        let key = &config.auth.api_keys[0];
        assert_eq!(key.name.as_deref(), Some("claude-code-local"));
        assert_eq!(
            key.key_hash,
            "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(key.actor_id, "claude-code");
        assert_eq!(key.created_at, "2026-04-12T00:00:00Z");
        assert!(key.revoked_at.is_none());
        let alias_key = &config.auth.api_keys[1];
        assert!(alias_key.name.is_none());
        assert_eq!(
            alias_key.key_hash,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert_eq!(alias_key.actor_id, "cursor");
        assert_eq!(config.server.bind.to_string(), "0.0.0.0");
        assert_eq!(config.server.port, 8443);
        assert_eq!(
            config
                .server
                .tls
                .cert
                .as_deref()
                .map(|path| path.display().to_string()),
            Some("/tmp/cert.pem".to_string())
        );
        assert_eq!(
            config
                .server
                .tls
                .key
                .as_deref()
                .map(|path| path.display().to_string()),
            Some("/tmp/key.pem".to_string())
        );
        Ok(())
    }

    #[test]
    fn explicit_config_path_overrides_home_default() {
        let home = PathBuf::from("/Users/example");
        let explicit = PathBuf::from("/tmp/ma-config.yaml");

        assert_eq!(
            resolve_config_path_from(Some(explicit.clone()), &home),
            explicit
        );
        assert_eq!(
            resolve_config_path_from(None, &home),
            home.join(".memra").join("config.yaml")
        );
    }

    #[test]
    fn invalid_yaml_returns_config_error() -> Result<(), String> {
        let path = temp_config_path("invalid")?;
        fs::write(&path, "auth: [").map_err(|error| error.to_string())?;

        let error = match AppConfig::load_from_path(&path) {
            Ok(_) => return Err("expected invalid YAML to fail".to_string()),
            Err(error) => error,
        };

        assert!(matches!(error, ConfigError::InvalidYaml { .. }));
        Ok(())
    }
}
