use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{parse_docker_api_timeout, parse_iso8601_duration, secret::Secret};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/nerd-backup/config.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub restic: ResticConfig,
    pub aws: AwsConfig,
    pub backup: BackupConfig,
    pub maintenance: Option<MaintenanceConfig>,
    pub volumes: Vec<VolumeBackupConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResticConfig {
    pub repository: String,
    pub password: Secret,
    pub tag_prefix: String,
    pub snapshot_retention: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    pub access_key_id: Secret,
    pub secret_access_key: Secret,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    pub interval: String,
    pub docker_api_timeout: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceConfig {
    pub marker_dir: PathBuf,
    pub marker_ttl: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(from = "VolumeConfigFile")]
pub struct VolumeBackupConfig {
    pub name: String,
    pub consistency: BackupConsistency,
}

#[derive(Debug, Clone)]
pub enum BackupConsistency {
    StopConsumers,
    AioBorgLock(AioBorgLockConfig),
}

// Use an internally tagged wire enum instead of flattening the strategy into
// the volume struct: Serde flatten cannot enforce unknown-field rejection.
#[derive(Deserialize)]
#[serde(tag = "consistency", rename_all = "kebab-case", deny_unknown_fields)]
enum VolumeConfigFile {
    StopConsumers {
        name: String,
    },
    AioBorgLock {
        name: String,
        aio: AioBorgLockConfig,
    },
}

impl From<VolumeConfigFile> for VolumeBackupConfig {
    fn from(config: VolumeConfigFile) -> Self {
        match config {
            VolumeConfigFile::StopConsumers { name } => Self {
                name,
                consistency: BackupConsistency::StopConsumers,
            },
            VolumeConfigFile::AioBorgLock { name, aio } => Self {
                name,
                consistency: BackupConsistency::AioBorgLock(aio),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AioBorgLockConfig {
    pub lockfile: PathBuf,
    pub running_marker_volume: String,
    pub running_marker_path: PathBuf,
    #[serde(deserialize_with = "deserialize_positive_duration")]
    pub wait_interval: Duration,
    #[serde(deserialize_with = "deserialize_positive_duration")]
    pub wait_timeout: Duration,
}

fn deserialize_positive_duration<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    let value = String::deserialize(deserializer)?;
    positive_duration(&value, "AIO wait duration").map_err(serde::de::Error::custom)
}

pub fn positive_duration(value: &str, field: &str) -> Result<Duration> {
    let duration = parse_iso8601_duration(value).with_context(|| format!("Invalid {field}"))?;
    if duration.is_zero() {
        bail!("{field} must be greater than zero");
    }
    Ok(duration)
}

impl Config {
    pub fn from_toml(source: &str) -> Result<Self> {
        // TOML errors normally include the source line, which may contain passwords.
        // Deserialize a Value first and suppress syntax details; shape errors have no
        // source excerpt. Redact scalar values even if a secret has the wrong type.
        let value: toml::Value = source.parse().map_err(|_: toml::de::Error| {
            anyhow::anyhow!(
                "Invalid TOML syntax in configuration (source omitted to protect secrets)"
            )
        })?;
        let mut values = Vec::new();
        collect_diagnostic_values(&value, &mut values);
        let config: Self = value.try_into().map_err(|error: toml::de::Error| {
            let secrets = values.iter().collect::<Vec<_>>();
            anyhow::anyhow!(
                "Invalid configuration: {}",
                crate::secret::redact(error.message(), &secrets)
            )
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn load() -> Result<Self> {
        // Explicit paths are authoritative: a typo must not silently fall back.
        let explicit = std::env::var_os("NERD_BACKUP_CONFIG").map(PathBuf::from);
        Self::load_from(explicit.as_deref(), Path::new(DEFAULT_CONFIG_PATH), || {
            let legacy: LegacyConfig = envy::prefixed("NERD_BACKUP_").from_env()
                .map_err(|_| anyhow::anyhow!("Invalid legacy NERD_BACKUP_* configuration; check required fields and types (values omitted to protect secrets)"))?;
            tracing::warn!(
                "No TOML configuration found; using legacy NERD_BACKUP_* settings with stop-consumers for all volumes"
            );
            Ok(legacy.into())
        })
    }

    fn load_from(
        explicit: Option<&Path>,
        default: &Path,
        fallback: impl FnOnce() -> Result<Self>,
    ) -> Result<Self> {
        let path = explicit.unwrap_or(default);
        match std::fs::read_to_string(path) {
            Ok(source) => Self::from_toml(&source).context("Failed to load TOML configuration"),
            Err(error) if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is configured but unreadable, not absent.
                // Falling back here could change AIO volumes to stop-consumers.
                match std::fs::symlink_metadata(path) {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(error).context("TOML configuration exists but cannot be read");
                    }
                    Err(error) => {
                        return Err(error).context("Failed to inspect TOML configuration file");
                    }
                }
                let config = fallback()?;
                config.validate()?;
                Ok(config)
            }
            Err(error) => Err(error).context("Failed to read TOML configuration file"),
        }
    }

    fn validate(&self) -> Result<()> {
        positive_duration(&self.backup.interval, "backup.interval")?;
        parse_docker_api_timeout(self.backup.docker_api_timeout.as_deref())?;
        if let Some(ttl) = self
            .maintenance
            .as_ref()
            .and_then(|maintenance| maintenance.marker_ttl.as_deref())
        {
            positive_duration(ttl, "maintenance.marker_ttl")?;
        }
        for volume in &self.volumes {
            if volume.name.is_empty() {
                bail!("Volume name must not be empty");
            }
            if let BackupConsistency::AioBorgLock(aio) = &volume.consistency {
                validate_relative_path(&aio.lockfile, "aio.lockfile")?;
                validate_relative_path(&aio.running_marker_path, "aio.running_marker_path")?;
                if aio.running_marker_volume.is_empty() {
                    bail!("aio.running_marker_volume must not be empty");
                }
            }
        }
        Ok(())
    }
}

fn validate_relative_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!("{field} must be a nonempty relative path without parent traversal");
    }
    Ok(())
}

fn collect_diagnostic_values(value: &toml::Value, values: &mut Vec<Secret>) {
    match value {
        toml::Value::String(value) => {
            values.push(value.clone().into());
            // Serde can quote/escape a rejected string value in its diagnostic.
            values.push(format!("{value:?}").into());
        }
        toml::Value::Array(items) => items
            .iter()
            .for_each(|v| collect_diagnostic_values(v, values)),
        toml::Value::Table(items) => items
            .values()
            .for_each(|v| collect_diagnostic_values(v, values)),
        _ => values.push(value.to_string().into()),
    }
}

#[derive(Deserialize, Debug)]
struct LegacyConfig {
    restic_repository: String,
    restic_password: Secret,
    aws_access_key_id: Secret,
    aws_secret_access_key: Secret,
    volumes_to_backup: Vec<String>,
    tag_prefix: String,
    backup_interval: String,
    snapshot_retention: Option<String>,
    docker_api_timeout: Option<String>,
    maintenance_marker_dir: Option<String>,
    maintenance_marker_ttl: Option<String>,
}

impl From<LegacyConfig> for Config {
    fn from(value: LegacyConfig) -> Self {
        Self {
            restic: ResticConfig {
                repository: value.restic_repository,
                password: value.restic_password,
                tag_prefix: value.tag_prefix,
                snapshot_retention: value.snapshot_retention,
            },
            aws: AwsConfig {
                access_key_id: value.aws_access_key_id,
                secret_access_key: value.aws_secret_access_key,
            },
            backup: BackupConfig {
                interval: value.backup_interval,
                docker_api_timeout: value.docker_api_timeout,
            },
            maintenance: value.maintenance_marker_dir.map(|dir| MaintenanceConfig {
                marker_dir: dir.into(),
                marker_ttl: value.maintenance_marker_ttl,
            }),
            volumes: value
                .volumes_to_backup
                .into_iter()
                .map(|name| VolumeBackupConfig {
                    name,
                    consistency: BackupConsistency::StopConsumers,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: &str = r#"
[restic]
repository = "s3:example/backups"
password = "test-restic-password"
tag_prefix = "daily-"
snapshot_retention = "P3D"
[aws]
access_key_id = "test-access-key"
secret_access_key = "test-secret-key"
[backup]
interval = "PT24H"
"#;
    const NORMAL: &str = "\n[[volumes]]\nname = 'ordinary'\nconsistency = 'stop-consumers'\n";
    const AIO: &str = r#"
[[volumes]]
name = "custom-borg"
consistency = "aio-borg-lock"
[volumes.aio]
lockfile = "borg/aio-lockfile"
running_marker_volume = "custom-dump"
running_marker_path = "backup-is-running"
wait_interval = "PT30S"
wait_timeout = "PT1H"
"#;
    #[test]
    fn ordinary_volume_needs_no_aio_fields() {
        let config = Config::from_toml(&format!("{BASE}{NORMAL}")).unwrap();
        assert!(matches!(
            config.volumes[0].consistency,
            BackupConsistency::StopConsumers
        ));
        assert_eq!(config.restic.snapshot_retention.as_deref(), Some("P3D"));
    }
    #[test]
    fn aio_volume_parses_with_custom_names_and_paths() {
        let config = Config::from_toml(&format!("{BASE}{AIO}")).unwrap();
        let BackupConsistency::AioBorgLock(aio) = &config.volumes[0].consistency else {
            panic!()
        };
        assert_eq!(aio.lockfile, Path::new("borg/aio-lockfile"));
        assert_eq!(aio.running_marker_volume, "custom-dump");
        assert_eq!(aio.wait_interval, Duration::from_secs(30));
        assert_eq!(aio.wait_timeout, Duration::from_secs(3600));
    }
    #[test]
    fn invalid_strategy_and_missing_aio_fields_fail_clearly() {
        let error = Config::from_toml(&format!(
            "{BASE}{}",
            NORMAL.replace("stop-consumers", "bad-strategy")
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown variant") && error.contains("aio-borg-lock"));
        for field in [
            "lockfile",
            "running_marker_volume",
            "running_marker_path",
            "wait_interval",
            "wait_timeout",
        ] {
            let aio = AIO
                .lines()
                .filter(|line| !line.starts_with(&format!("{field} =")))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                Config::from_toml(&format!("{BASE}{aio}"))
                    .unwrap_err()
                    .to_string()
                    .contains(field)
            );
        }
        assert!(
            Config::from_toml(&format!(
                "{BASE}{}",
                NORMAL.replace("stop-consumers", "aio-borg-lock")
            ))
            .is_err()
        );
    }
    #[test]
    fn zero_durations_and_unsafe_paths_are_rejected() {
        for (old, new) in [
            ("PT30S", "PT0S"),
            ("PT1H", "PT0S"),
            ("borg/aio-lockfile", "../aio-lockfile"),
            ("backup-is-running", "/tmp/running"),
        ] {
            assert!(Config::from_toml(&format!("{BASE}{}", AIO.replace(old, new))).is_err());
        }
        assert!(Config::from_toml(&format!("{}{NORMAL}", BASE.replace("PT24H", "PT0S"))).is_err());
    }
    #[test]
    fn secrets_are_redacted_from_debug_and_parse_errors() {
        let source = format!("{BASE}{NORMAL}");
        let config = Config::from_toml(&source).unwrap();
        let debug = format!("{config:?}");
        assert_eq!(debug.matches(crate::secret::REDACTED).count(), 3);
        let errors = [
            Config::from_toml(&source.replace("password =", "password_typo =")).unwrap_err(),
            Config::from_toml(&source.replace(
                "password = \"test-restic-password\"",
                "password = [\"test-restic-password\"]",
            ))
            .unwrap_err(),
            Config::from_toml(&source.replace("password =", "password = !")).unwrap_err(),
        ];
        for secret in ["test-restic-password", "test-access-key", "test-secret-key"] {
            assert!(!debug.contains(secret));
            for error in &errors {
                assert!(!format!("{error:?}").contains(secret));
            }
        }
    }
    #[test]
    fn config_path_precedence_and_no_fallback_for_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("default.toml");
        let explicit = dir.path().join("explicit.toml");
        std::fs::write(&default, format!("{BASE}{NORMAL}")).unwrap();
        std::fs::write(&explicit, format!("{BASE}{AIO}")).unwrap();
        let no_fallback = || -> Result<Config> { panic!("unexpected fallback") };
        assert_eq!(
            Config::load_from(Some(&explicit), &default, no_fallback)
                .unwrap()
                .volumes[0]
                .name,
            "custom-borg"
        );
        assert_eq!(
            Config::load_from(None, &default, no_fallback)
                .unwrap()
                .volumes[0]
                .name,
            "ordinary"
        );
        std::fs::remove_file(&explicit).unwrap();
        assert!(Config::load_from(Some(&explicit), &default, no_fallback).is_err());
        std::fs::write(&default, "invalid").unwrap();
        assert!(Config::load_from(None, &default, no_fallback).is_err());
        std::fs::remove_file(&default).unwrap();
        assert!(
            Config::load_from(None, &default, || Config::from_toml(&format!(
                "{BASE}{NORMAL}"
            )))
            .is_ok()
        );
    }

    #[test]
    fn dangling_default_config_symlink_must_not_enable_legacy_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("config.toml");
        std::os::unix::fs::symlink(dir.path().join("missing-target.toml"), &default).unwrap();
        let result = Config::load_from(None, &default, || {
            panic!("a broken TOML configuration must not switch consistency strategies")
        });
        assert!(result.is_err());
    }
    #[test]
    fn legacy_config_maps_every_volume_to_stop_consumers() {
        let legacy: LegacyConfig = serde_json::from_value(serde_json::json!({
            "restic_repository": "s3:example", "restic_password": "password",
            "aws_access_key_id": "key", "aws_secret_access_key": "secret",
            "volumes_to_backup": ["data", "db"], "tag_prefix": "daily-",
            "backup_interval": "PT24H", "snapshot_retention": "P3D",
            "maintenance_marker_dir": "/markers", "maintenance_marker_ttl": "PT1H"
        }))
        .unwrap();
        let config: Config = legacy.into();
        config.validate().unwrap();
        assert!(
            config
                .volumes
                .iter()
                .all(|v| matches!(v.consistency, BackupConsistency::StopConsumers))
        );
        assert_eq!(
            config.maintenance.unwrap().marker_dir,
            Path::new("/markers")
        );
    }
    #[test]
    fn example_config_matches_the_supported_contract() {
        let config = Config::from_toml(include_str!("../config.example.toml")).unwrap();
        assert_eq!(config.volumes.len(), 2);
        assert!(matches!(
            config.volumes[0].consistency,
            BackupConsistency::StopConsumers
        ));
        assert!(matches!(
            config.volumes[1].consistency,
            BackupConsistency::AioBorgLock(_)
        ));
    }

    #[test]
    fn misspelled_fields_and_irrelevant_aio_tables_are_rejected() {
        for suffix in [
            NORMAL.replace("consistency =", "consistency_typo ="),
            format!("{NORMAL}unexpected = 'value'"),
            format!("{NORMAL}[volumes.aio]\nlockfile = 'aio-lockfile'"),
            AIO.replace("lockfile =", "lockfile_typo ="),
        ] {
            assert!(Config::from_toml(&format!("{BASE}{suffix}")).is_err());
        }
    }

    #[test]
    fn malformed_numeric_and_escaped_credentials_are_redacted_from_errors() {
        let numeric = BASE.replace(
            "password = \"test-restic-password\"",
            "password = 123456789",
        );
        let error = Config::from_toml(&format!("{numeric}{NORMAL}")).unwrap_err();
        assert!(!format!("{error:?}").contains("123456789"));
        let escaped = BASE.replace(
            "password = \"test-restic-password\"",
            r#"password = ["secret\"quoted"]"#,
        );
        let error = Config::from_toml(&format!("{escaped}{NORMAL}")).unwrap_err();
        assert!(!format!("{error:?}").contains("secret"));
    }
}
