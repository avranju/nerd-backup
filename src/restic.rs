use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    process::Stdio,
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

use bollard::query_parameters::{
    ListContainersOptions, StartContainerOptions, StopContainerOptions,
};
use humantime::format_rfc3339_seconds;
use iso8601::Duration;
use serde::Serialize;
use tokio::process::Command;

use crate::error::{CheckError, Error};

const MAINTENANCE_REASON: &str = "volume backup";

#[derive(Debug, Clone)]
pub enum Backend {
    S3 {
        access_key_id: String,
        secret_access_key: String,
    },
}

#[derive(Debug)]
pub struct Restic {
    repository: String,
    password: String,
    backend: Backend,
    tag_prefix: String,
    snapshot_retention: Option<String>,
    docker_api_timeout: StdDuration,
    maintenance_markers: Option<MaintenanceMarkerConfig>,
}

#[derive(Debug, Clone)]
pub struct MaintenanceMarkerConfig {
    directory: PathBuf,
    ttl: StdDuration,
}

impl MaintenanceMarkerConfig {
    pub fn new(directory: impl Into<PathBuf>, ttl: StdDuration) -> Self {
        Self {
            directory: directory.into(),
            ttl,
        }
    }
}

impl Restic {
    pub fn new(
        repository: String,
        password: String,
        backend: Backend,
        tag_prefix: String,
        snapshot_retention: Option<String>,
        docker_api_timeout: StdDuration,
        maintenance_markers: Option<MaintenanceMarkerConfig>,
    ) -> Self {
        Restic {
            repository,
            password,
            backend,
            tag_prefix,
            snapshot_retention,
            docker_api_timeout,
            maintenance_markers,
        }
    }

    #[tracing::instrument]
    pub async fn init(&self) -> Result<(), Error> {
        match self.check().await {
            Ok(_) => {
                tracing::info!("Repository already initialized at {}", self.repository);
                Ok(())
            }
            Err(Error::Check(CheckError::Locked)) => {
                self.unlock().await?;
                Ok(())
            }
            Err(Error::Check(CheckError::NotFound)) => {
                tracing::info!("Initializing new repository at {}", self.repository);

                let mut cmd = self.build_command();
                cmd.stdout(Stdio::null()).stderr(Stdio::null()).arg("init");
                let child = cmd.spawn()?;
                let output = child.wait_with_output().await?;

                if !output.status.success() {
                    tracing::error!(
                        "Failed to initialize repository: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    return Err(Error::Init);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    #[tracing::instrument]
    pub async fn check(&self) -> Result<(), Error> {
        tracing::info!("Checking repository status at {}", self.repository);

        let mut cmd = self.build_command();
        cmd.stdout(Stdio::null()).stderr(Stdio::null()).arg("check");

        let child = cmd.spawn()?;
        let output = child.wait_with_output().await?;

        if !output.status.success() {
            let code = output.status.code().unwrap_or(1);
            return Err(Error::Check(CheckError::from(code)));
        }

        Ok(())
    }

    #[tracing::instrument]
    pub async fn unlock(&self) -> Result<(), Error> {
        tracing::info!("Unlocking repository at {}", self.repository);

        let mut cmd = self.build_command();
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .arg("unlock");

        let child = cmd.spawn()?;
        let output = child.wait_with_output().await?;

        if !output.status.success() {
            output.status.code().unwrap_or(1);
            return Err(Error::Unlock(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(())
    }

    #[tracing::instrument]
    pub async fn backup(&self, volumes: Vec<String>) -> Result<(), Error> {
        let docker = bollard::Docker::connect_with_socket(
            "/var/run/docker.sock",
            self.docker_api_timeout.as_secs(),
            bollard::API_DEFAULT_VERSION,
        )?;

        for vol in volumes {
            let vol_info = docker.inspect_volume(&vol).await?;
            tracing::info!("Backing up {}", vol_info.name);

            // Find container(s) to which this volume is attached
            let filters = HashMap::from([("volume".to_string(), vec![vol_info.name.clone()])]);
            let options = ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            };
            let containers = docker.list_containers(Some(options)).await?;

            let maintenance_markers =
                MaintenanceMarkers::create(self.maintenance_markers.as_ref(), &containers)?;

            // Stop all containers using this volume
            if let Err(e) = stop_containers(&docker, &containers).await {
                maintenance_markers.delete_all_best_effort();
                return Err(e);
            }

            let res = self.do_backup(vol_info).await;

            // Start all containers again
            if let Err(e) = start_containers(&docker, &containers).await {
                maintenance_markers.delete_all_best_effort();
                return Err(e);
            }

            if let Err(e) = maintenance_markers.delete_all() {
                if res.is_err() {
                    tracing::warn!("Failed to delete maintenance marker(s): {}", e);
                } else {
                    return Err(e);
                }
            }

            res?;
        }
        Ok(())
    }

    async fn do_backup(&self, vol_info: bollard::secret::Volume) -> Result<(), Error> {
        let mut cmd = self.build_command();
        cmd.arg("backup")
            .arg("--tag")
            .arg(format!("{}{}", self.tag_prefix, vol_info.name))
            .arg(vol_info.mountpoint);
        let child = cmd.spawn()?;
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            tracing::error!(
                "Failed to backup {}: {}",
                vol_info.name,
                String::from_utf8_lossy(&output.stderr)
            );
            return Err(Error::Backup(
                vol_info.name.clone(),
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        tracing::info!("Backup completed for {}", vol_info.name);
        Ok(())
    }

    #[tracing::instrument]
    pub async fn prune_snapshots(&self) -> Result<(), Error> {
        if let Some(retention) = &self.snapshot_retention {
            tracing::info!("Pruning snapshots older than: {}", retention);

            // Convert ISO 8601 duration to restic format
            let restic_duration = convert_iso8601_to_restic_format(retention)
                .map_err(|e| Error::Prune(format!("Failed to parse retention duration: {}", e)))?;

            // Use restic forget command with the duration-based retention
            let mut cmd = self.build_command();
            cmd.arg("forget")
                .arg("--prune")
                .arg("--keep-within")
                .arg(&restic_duration);

            let child = cmd.spawn()?;
            let output = child.wait_with_output().await?;

            if !output.status.success() {
                let error_msg = String::from_utf8_lossy(&output.stderr);
                tracing::error!("Failed to prune snapshots: {}", error_msg);
                return Err(Error::Prune(error_msg.to_string()));
            }

            tracing::info!("Successfully pruned old snapshots");
        } else {
            tracing::debug!("No snapshot retention configured, skipping pruning");
        }

        Ok(())
    }

    fn build_command(&self) -> Command {
        let mut cmd = Command::new("restic");
        cmd.env("RESTIC_REPOSITORY", &self.repository)
            .env("RESTIC_PASSWORD", &self.password);

        match &self.backend {
            Backend::S3 {
                access_key_id,
                secret_access_key,
            } => cmd
                .env("AWS_ACCESS_KEY_ID", access_key_id)
                .env("AWS_SECRET_ACCESS_KEY", secret_access_key),
        };

        cmd
    }
}

#[derive(Serialize)]
struct MaintenanceMarker {
    expires_at: String,
    reason: &'static str,
}

struct MaintenanceMarkers {
    paths: Vec<PathBuf>,
}

impl MaintenanceMarkers {
    fn create(
        config: Option<&MaintenanceMarkerConfig>,
        containers: &[bollard::secret::ContainerSummary],
    ) -> Result<Self, Error> {
        let Some(config) = config else {
            return Ok(Self { paths: Vec::new() });
        };

        fs::create_dir_all(&config.directory)?;
        let mut paths = Vec::new();

        for container in containers {
            let Some(container_name) = container_marker_name(container) else {
                tracing::warn!(
                    "Skipping maintenance marker for unnamed container {:?}",
                    container.id
                );
                continue;
            };

            match write_maintenance_marker(config, &container_name) {
                Ok(path) => paths.push(path),
                Err(e) => {
                    MaintenanceMarkers { paths }.delete_all_best_effort();
                    return Err(e);
                }
            }
        }

        Ok(Self { paths })
    }

    fn delete_all(&self) -> Result<(), Error> {
        for path in &self.paths {
            match fs::remove_file(path) {
                Ok(_) => tracing::info!("Deleted maintenance marker {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    fn delete_all_best_effort(&self) {
        if let Err(e) = self.delete_all() {
            tracing::warn!("Failed to delete maintenance marker(s): {}", e);
        }
    }
}

fn container_marker_name(container: &bollard::secret::ContainerSummary) -> Option<String> {
    container
        .names
        .as_ref()?
        .iter()
        .find_map(|name| {
            let name = name.strip_prefix('/').unwrap_or(name.as_str());
            (!name.is_empty()).then_some(name)
        })
        .map(ToOwned::to_owned)
}

fn write_maintenance_marker(
    config: &MaintenanceMarkerConfig,
    container_name: &str,
) -> Result<PathBuf, Error> {
    let marker = MaintenanceMarker {
        expires_at: format_rfc3339_seconds(SystemTime::now() + config.ttl).to_string(),
        reason: MAINTENANCE_REASON,
    };
    let contents = serde_json::to_vec_pretty(&marker)?;
    let path = config.directory.join(format!("{container_name}.json"));
    let temp_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temp_path = config.directory.join(format!(
        ".{container_name}.json.{}.{temp_suffix}.tmp",
        std::process::id()
    ));

    let mut temp_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    temp_file.write_all(&contents)?;
    temp_file.sync_all()?;
    drop(temp_file);

    fs::rename(&temp_path, &path)?;
    tracing::info!("Created maintenance marker {}", path.display());

    Ok(path)
}

async fn stop_containers(
    docker: &bollard::Docker,
    containers: &[bollard::secret::ContainerSummary],
) -> Result<(), Error> {
    for container in containers {
        if let Some(container_id) = container.id.as_ref() {
            tracing::info!("Stopping container {:?}.", container.names);
            docker
                .stop_container(container_id, Option::<StopContainerOptions>::None)
                .await?;
            tracing::info!("Stopped container {:?}.", container.names);
        }
    }

    Ok(())
}

async fn start_containers(
    docker: &bollard::Docker,
    containers: &[bollard::secret::ContainerSummary],
) -> Result<(), Error> {
    for container in containers {
        if let Some(container_id) = container.id.as_ref() {
            tracing::info!("Starting container {:?}.", container.names);
            docker
                .start_container(container_id, Option::<StartContainerOptions>::None)
                .await?;
            tracing::info!("Started container {:?}.", container.names);
        }
    }

    Ok(())
}

/// Convert ISO 8601 duration to restic --keep-within format
/// Examples: P3D -> 3d, P1W -> 7d, P1M -> 30d, P1Y -> 365d
fn convert_iso8601_to_restic_format(iso_duration: &str) -> Result<String, String> {
    let duration = iso_duration
        .parse::<Duration>()
        .map_err(|e| format!("Invalid ISO 8601 duration: {:?}", e))?;

    // Convert duration to total days and use restic's day format
    let std_duration: std::time::Duration = duration.into();
    let total_days = std_duration.as_secs() / (24 * 60 * 60);

    if total_days == 0 {
        // For sub-day durations, convert to hours
        let total_hours = std_duration.as_secs() / (60 * 60);
        if total_hours == 0 {
            // For sub-hour durations, convert to minutes
            let total_minutes = std_duration.as_secs() / 60;
            if total_minutes == 0 {
                return Err("Duration too short (less than 1 minute)".to_string());
            }
            Ok(format!("{}m", total_minutes))
        } else {
            Ok(format!("{}h", total_hours))
        }
    } else {
        Ok(format!("{}d", total_days))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn temp_test_dir(test_name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nerd-backup-{test_name}-{}-{now}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn container(name: &str) -> bollard::secret::ContainerSummary {
        bollard::secret::ContainerSummary {
            id: Some(format!("{name}-id")),
            names: Some(vec![format!("/{name}")]),
            ..Default::default()
        }
    }

    #[test]
    fn marker_json_contains_reason_and_rfc3339_expiration() {
        let dir = temp_test_dir("json");
        let ttl = StdDuration::from_secs(60 * 60);
        let config = MaintenanceMarkerConfig::new(&dir, ttl);
        let before = SystemTime::now();

        let path = write_maintenance_marker(&config, "my-app").unwrap();
        let json: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let expires_at = json
            .get("expires_at")
            .and_then(Value::as_str)
            .expect("expires_at must be a string");
        let parsed_expires_at = humantime::parse_rfc3339(expires_at).unwrap();

        assert_eq!(
            json.get("reason").and_then(Value::as_str),
            Some("volume backup")
        );
        assert!(expires_at.ends_with('Z'));
        assert!(parsed_expires_at > before);
        assert!(parsed_expires_at <= SystemTime::now() + ttl);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn marker_write_renames_temp_file_to_final_marker() {
        let dir = temp_test_dir("atomic");
        let config = MaintenanceMarkerConfig::new(&dir, StdDuration::from_secs(60 * 60));

        let path = write_maintenance_marker(&config, "my-app").unwrap();
        let entries = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(path, dir.join("my-app.json"));
        assert!(path.exists());
        assert_eq!(entries, vec!["my-app.json"]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn markers_delete_only_created_marker_files_after_success() {
        let dir = temp_test_dir("delete-success");
        let unrelated = dir.join("unrelated.json");
        fs::write(&unrelated, "{}").unwrap();
        let config = MaintenanceMarkerConfig::new(&dir, StdDuration::from_secs(60 * 60));
        let containers = vec![container("my-app")];

        let markers = MaintenanceMarkers::create(Some(&config), &containers).unwrap();
        markers.delete_all().unwrap();

        assert!(!dir.join("my-app.json").exists());
        assert!(unrelated.exists());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn markers_cleanup_best_effort_removes_created_markers_on_error_path() {
        let dir = temp_test_dir("delete-error");
        let unrelated = dir.join("other-app.json");
        fs::write(&unrelated, "{}").unwrap();
        let config = MaintenanceMarkerConfig::new(&dir, StdDuration::from_secs(60 * 60));
        let containers = vec![container("my-app")];

        let markers = MaintenanceMarkers::create(Some(&config), &containers).unwrap();
        markers.delete_all_best_effort();

        assert!(!dir.join("my-app.json").exists());
        assert!(unrelated.exists());

        fs::remove_dir_all(dir).unwrap();
    }
}
