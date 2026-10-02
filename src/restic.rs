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

use crate::{
    config::{BackupConsistency, ResticConfig, VolumeBackupConfig},
    error::{CheckError, Error},
    secret::{Secret, redact},
    shutdown::Shutdown,
};

const MAINTENANCE_REASON: &str = "volume backup";

#[derive(Debug, Clone)]
pub enum Backend {
    S3 {
        access_key_id: Secret,
        secret_access_key: Secret,
    },
}

#[derive(Debug)]
pub struct Restic {
    repository: String,
    password: Secret,
    backend: Backend,
    tag_prefix: String,
    snapshot_retention: Option<String>,
    docker_api_timeout: StdDuration,
    maintenance_markers: Option<MaintenanceMarkerConfig>,
    shutdown: Shutdown,
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
        config: ResticConfig,
        backend: Backend,
        docker_api_timeout: StdDuration,
        maintenance_markers: Option<MaintenanceMarkerConfig>,
        shutdown: Shutdown,
    ) -> Self {
        Restic {
            repository: config.repository,
            password: config.password,
            backend,
            tag_prefix: config.tag_prefix,
            snapshot_retention: config.snapshot_retention,
            docker_api_timeout,
            maintenance_markers,
            shutdown,
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
                cmd.stdout(Stdio::null()).arg("init");
                let output = run_command(cmd, &self.shutdown).await?;

                if !output.status.success() {
                    tracing::error!(
                        "Failed to initialize repository: {}",
                        self.redact_output(&output.stderr)
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

        let output = run_command(cmd, &self.shutdown).await?;

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
        cmd.stdout(Stdio::null()).arg("unlock");

        let output = run_command(cmd, &self.shutdown).await?;

        if !output.status.success() {
            output.status.code().unwrap_or(1);
            return Err(Error::Unlock(self.redact_output(&output.stderr)));
        }

        Ok(())
    }

    #[tracing::instrument]
    pub async fn backup(&self, volumes: &[VolumeBackupConfig]) -> Result<(), Error> {
        let docker = bollard::Docker::connect_with_socket(
            "/var/run/docker.sock",
            self.docker_api_timeout.as_secs(),
            bollard::API_DEFAULT_VERSION,
        )?;
        let operations = DockerBackup {
            docker,
            restic: self,
        };
        for volume in volumes {
            backup_volume(
                &operations,
                volume,
                self.maintenance_markers.as_ref(),
                &self.shutdown,
            )
            .await?;
        }
        Ok(())
    }

    async fn do_backup(&self, vol_info: bollard::secret::Volume) -> Result<(), Error> {
        let mut cmd = self.build_command();
        cmd.arg("backup")
            .arg("--tag")
            .arg(format!("{}{}", self.tag_prefix, vol_info.name))
            .arg(vol_info.mountpoint);
        let output = run_command(cmd, &self.shutdown).await?;
        self.log_output(&output);
        if !output.status.success() {
            let stderr = self.redact_output(&output.stderr);
            tracing::error!("Failed to backup {}: {}", vol_info.name, stderr);
            return Err(Error::Backup(vol_info.name.clone(), stderr));
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

            let output = run_command(cmd, &self.shutdown).await?;

            self.log_output(&output);
            if !output.status.success() {
                let error_msg = self.redact_output(&output.stderr);
                tracing::error!("Failed to prune snapshots: {}", error_msg);
                return Err(Error::Prune(error_msg));
            }

            tracing::info!("Successfully pruned old snapshots");
        } else {
            tracing::debug!("No snapshot retention configured, skipping pruning");
        }

        Ok(())
    }

    fn redact_output(&self, output: &[u8]) -> String {
        let Backend::S3 {
            access_key_id,
            secret_access_key,
        } = &self.backend;
        redact(
            &String::from_utf8_lossy(output),
            &[&self.password, access_key_id, secret_access_key],
        )
    }

    fn log_output(&self, output: &std::process::Output) {
        if !output.stdout.is_empty() {
            tracing::info!("{}", self.redact_output(&output.stdout));
        }
        if !output.stderr.is_empty() {
            tracing::warn!("{}", self.redact_output(&output.stderr));
        }
    }

    fn build_command(&self) -> Command {
        let mut cmd = Command::new("restic");
        // Never inherit output: restic may echo credentials in diagnostics.
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.env("RESTIC_REPOSITORY", &self.repository)
            .env("RESTIC_PASSWORD", self.password.expose());

        match &self.backend {
            Backend::S3 {
                access_key_id,
                secret_access_key,
            } => cmd
                .env("AWS_ACCESS_KEY_ID", access_key_id.expose())
                .env("AWS_SECRET_ACCESS_KEY", secret_access_key.expose()),
        };

        cmd
    }
}

// A small operations boundary lets strategy tests verify the actual orchestration
// without a Docker daemon, live volumes, AWS credentials, or a Restic repository.
trait BackupOperations {
    async fn inspect(&self, name: &str) -> Result<bollard::secret::Volume, Error>;
    async fn consumers(&self, name: &str) -> Result<Vec<bollard::secret::ContainerSummary>, Error>;
    async fn stop(&self, containers: &[bollard::secret::ContainerSummary]) -> Result<(), Error>;
    async fn start(&self, containers: &[bollard::secret::ContainerSummary]) -> Result<(), Error>;
    async fn backup(&self, volume: bollard::secret::Volume) -> Result<(), Error>;
}

struct DockerBackup<'a> {
    docker: bollard::Docker,
    restic: &'a Restic,
}

impl BackupOperations for DockerBackup<'_> {
    async fn inspect(&self, name: &str) -> Result<bollard::secret::Volume, Error> {
        // Inspection is read-only and can be interrupted safely before stopping
        // any containers or acquiring a filesystem lock.
        tokio::select! {
            biased;
            _ = self.restic.shutdown.cancelled() => Err(Error::Cancelled),
            volume = self.docker.inspect_volume(name) => Ok(volume?),
        }
    }
    async fn consumers(&self, name: &str) -> Result<Vec<bollard::secret::ContainerSummary>, Error> {
        let filters = HashMap::from([("volume".to_string(), vec![name.to_owned()])]);
        let options = ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        };
        tokio::select! {
            biased;
            _ = self.restic.shutdown.cancelled() => Err(Error::Cancelled),
            containers = self.docker.list_containers(Some(options)) => Ok(containers?),
        }
    }
    async fn stop(&self, containers: &[bollard::secret::ContainerSummary]) -> Result<(), Error> {
        stop_containers(&self.docker, containers).await
    }
    async fn start(&self, containers: &[bollard::secret::ContainerSummary]) -> Result<(), Error> {
        // Cleanup must finish even after shutdown was requested.
        start_containers(&self.docker, containers).await
    }
    async fn backup(&self, volume: bollard::secret::Volume) -> Result<(), Error> {
        self.restic.do_backup(volume).await
    }
}

async fn backup_volume(
    operations: &impl BackupOperations,
    config: &VolumeBackupConfig,
    markers: Option<&MaintenanceMarkerConfig>,
    shutdown: &Shutdown,
) -> Result<(), Error> {
    if shutdown.is_requested() {
        return Err(Error::Cancelled);
    }
    let volume = operations.inspect(&config.name).await?;
    match &config.consistency {
        BackupConsistency::StopConsumers => {
            tracing::info!(volume = %config.name, consistency = "stop-consumers", "Backing up volume");
            let containers = operations.consumers(&volume.name).await?;
            if shutdown.is_requested() {
                return Err(Error::Cancelled);
            }
            let maintenance = MaintenanceMarkers::create(markers, &containers)?;
            if let Err(error) = operations.stop(&containers).await {
                maintenance.delete_all_best_effort();
                return Err(error);
            }
            let result = operations.backup(volume).await;
            if let Err(error) = operations.start(&containers).await {
                maintenance.delete_all_best_effort();
                return Err(error);
            }
            if let Err(error) = maintenance.delete_all() {
                if result.is_err() {
                    tracing::warn!("Failed to delete maintenance marker(s): {}", error);
                } else {
                    return Err(error);
                }
            }
            result
        }
        BackupConsistency::AioBorgLock(aio) => {
            tracing::info!(volume = %config.name, consistency = "aio-borg-lock", "Backing up volume");
            let running_volume = operations.inspect(&aio.running_marker_volume).await?;
            let lock_path = crate::aio::signal_path(&volume.mountpoint, &aio.lockfile)?;
            let running_path =
                crate::aio::signal_path(&running_volume.mountpoint, &aio.running_marker_path)?;
            let mut guard =
                crate::aio::acquire(&config.name, &lock_path, &running_path, aio, shutdown).await?;
            // run_command kills and reaps a cancelled Restic child before this
            // scope can release its AIO lock. Do not select/drop this future.
            let result = operations.backup(volume).await;
            if let Err(error) = guard.release() {
                if result.is_err() {
                    tracing::warn!(%error, "Failed to release AIO lock after backup failure");
                } else {
                    return Err(error.into());
                }
            }
            result
        }
    }
}

async fn run_command(
    mut command: Command,
    shutdown: &Shutdown,
) -> Result<std::process::Output, Error> {
    if shutdown.is_requested() {
        return Err(Error::Cancelled);
    }
    command.kill_on_drop(true);
    let mut child = command.spawn()?;
    // Drain both pipes while waiting; waiting first could deadlock on full pipes.
    let stdout = tokio::spawn(read_pipe(child.stdout.take()));
    let stderr = tokio::spawn(read_pipe(child.stderr.take()));
    let status = tokio::select! {
        biased;
        _ = shutdown.cancelled() => None,
        status = child.wait() => Some(status),
    };
    let cancelled = status.is_none();
    let status = match status {
        Some(status) => status,
        None => {
            tracing::info!("Stopping Restic subprocess for graceful shutdown");
            // kill() also waits for termination. Keep the AIO guard alive until
            // this completes, so AIO cannot start while Restic is still reading.
            if let Err(error) = child.kill().await {
                tracing::warn!(%error, "Failed to kill Restic; retaining consistency protection until the child exits");
            }
            child.wait().await
        }
    };
    let stdout = stdout.await.map_err(std::io::Error::other)??;
    let stderr = stderr.await.map_err(std::io::Error::other)??;
    if cancelled {
        return Err(Error::Cancelled);
    }
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}

async fn read_pipe(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
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
    use std::{
        os::unix::{fs::PermissionsExt, process::ExitStatusExt},
        sync::{Arc, Mutex},
    };

    fn test_restic() -> Restic {
        Restic::new(
            ResticConfig {
                repository: "s3:https://example.com/backups".to_string(),
                password: "test-restic-password".to_string().into(),
                tag_prefix: "backup-".to_string(),
                snapshot_retention: None,
            },
            Backend::S3 {
                access_key_id: "test-access-key".to_string().into(),
                secret_access_key: "test-secret-key".to_string().into(),
            },
            StdDuration::from_secs(60),
            None,
            Shutdown::new(),
        )
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn tracing_and_command_output_redact_credentials() {
        let restic = test_restic();
        let logs = LogBuffer::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        // Exercise a real instrumented method without invoking restic or Docker.
        restic.prune_snapshots().await.unwrap();
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: b"password=test-restic-password key=test-access-key\n".to_vec(),
            stderr: b"Authentication failed: test-secret-key\n".to_vec(),
        };
        restic.log_output(&output);
        let error = Error::Prune(restic.redact_output(&output.stderr));
        tracing::error!("{error}");

        let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        for credential in ["test-restic-password", "test-access-key", "test-secret-key"] {
            assert!(!logs.contains(credential), "credential leaked: {logs}");
            assert!(!format!("{restic:?}").contains(credential));
            assert!(!format!("{:?}", restic.backend).contains(credential));
            assert!(!format!("{error:?}").contains(credential));
        }
        assert!(logs.contains("password: [REDACTED]"));
        assert!(logs.contains("access_key_id: [REDACTED]"));
        assert!(logs.contains("secret_access_key: [REDACTED]"));
        assert!(logs.contains("password=[REDACTED] key=[REDACTED]"));
        assert!(logs.contains("Authentication failed: [REDACTED]"));
        assert!(logs.contains("s3:https://example.com/backups"));
    }

    #[test]
    fn command_environment_receives_unredacted_credentials() {
        let restic = test_restic();
        let command = restic.build_command();
        let env: HashMap<_, _> = command.as_std().get_envs().collect();
        for (key, value) in [
            ("RESTIC_REPOSITORY", "s3:https://example.com/backups"),
            ("RESTIC_PASSWORD", "test-restic-password"),
            ("AWS_ACCESS_KEY_ID", "test-access-key"),
            ("AWS_SECRET_ACCESS_KEY", "test-secret-key"),
        ] {
            assert_eq!(
                env[std::ffi::OsStr::new(key)],
                Some(std::ffi::OsStr::new(value))
            );
        }
    }

    #[tokio::test]
    async fn child_output_is_captured_for_redaction() {
        let dir = temp_test_dir("command-output");
        let executable = dir.join("restic");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$RESTIC_PASSWORD\" \"$AWS_ACCESS_KEY_ID\"\nprintf '%s\\n' \"$AWS_SECRET_ACCESS_KEY\" >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let restic = test_restic();
        let mut command = restic.build_command();
        // Override only this child's PATH; do not change the test process environment.
        command.env("PATH", &dir);
        let output = command.spawn().unwrap().wait_with_output().await.unwrap();

        assert!(!output.status.success());
        assert_eq!(output.stdout, b"test-restic-password\ntest-access-key\n");
        assert_eq!(output.stderr, b"test-secret-key\n");
        assert_eq!(
            restic.redact_output(&output.stdout),
            "[REDACTED]\n[REDACTED]\n"
        );
        assert_eq!(restic.redact_output(&output.stderr), "[REDACTED]\n");

        fs::remove_dir_all(dir).unwrap();
    }

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
    #[derive(Clone, Copy)]
    enum BackupOutcome {
        Success,
        Failure,
        Cancelled,
    }

    struct FakeOperations {
        borg_dir: PathBuf,
        dump_dir: PathBuf,
        events: std::cell::RefCell<Vec<String>>,
        outcome: BackupOutcome,
        aio: bool,
        markers: PathBuf,
    }

    impl BackupOperations for FakeOperations {
        async fn inspect(&self, name: &str) -> Result<bollard::secret::Volume, Error> {
            self.events.borrow_mut().push(format!("inspect:{name}"));
            Ok(bollard::secret::Volume {
                name: name.to_owned(),
                mountpoint: if name == "custom-dump" {
                    &self.dump_dir
                } else {
                    &self.borg_dir
                }
                .to_string_lossy()
                .into_owned(),
                ..Default::default()
            })
        }
        async fn consumers(
            &self,
            _: &str,
        ) -> Result<Vec<bollard::secret::ContainerSummary>, Error> {
            assert!(!self.aio, "AIO must not even enumerate consumers");
            self.events.borrow_mut().push("consumers".into());
            Ok(vec![container("my-app")])
        }
        async fn stop(&self, _: &[bollard::secret::ContainerSummary]) -> Result<(), Error> {
            assert!(!self.aio);
            assert!(self.markers.join("my-app.json").exists());
            self.events.borrow_mut().push("stop".into());
            Ok(())
        }
        async fn start(&self, _: &[bollard::secret::ContainerSummary]) -> Result<(), Error> {
            assert!(!self.aio);
            assert!(self.markers.join("my-app.json").exists());
            self.events.borrow_mut().push("start".into());
            Ok(())
        }
        async fn backup(&self, volume: bollard::secret::Volume) -> Result<(), Error> {
            assert_eq!(PathBuf::from(volume.mountpoint), self.borg_dir);
            if self.aio {
                assert!(self.borg_dir.join("aio-lockfile").exists());
                assert!(!self.dump_dir.join("backup-is-running").exists());
                assert!(
                    !self.markers.exists(),
                    "AIO must not create maintenance markers"
                );
            }
            self.events.borrow_mut().push("backup".into());
            match self.outcome {
                BackupOutcome::Success => Ok(()),
                BackupOutcome::Failure => Err(Error::Backup(volume.name, "test failure".into())),
                BackupOutcome::Cancelled => Err(Error::Cancelled),
            }
        }
    }

    fn fake_operations(dir: &std::path::Path, aio: bool, outcome: BackupOutcome) -> FakeOperations {
        let borg_dir = dir.join("borg");
        let dump_dir = dir.join("dump");
        fs::create_dir_all(&borg_dir).unwrap();
        fs::create_dir_all(&dump_dir).unwrap();
        FakeOperations {
            borg_dir,
            dump_dir,
            markers: dir.join("markers"),
            events: Default::default(),
            outcome,
            aio,
        }
    }

    fn aio_volume() -> VolumeBackupConfig {
        VolumeBackupConfig {
            name: "custom-borg".into(),
            consistency: BackupConsistency::AioBorgLock(crate::config::AioBorgLockConfig {
                lockfile: "aio-lockfile".into(),
                running_marker_volume: "custom-dump".into(),
                running_marker_path: "backup-is-running".into(),
                wait_interval: StdDuration::from_secs(30),
                wait_timeout: StdDuration::from_secs(60),
            }),
        }
    }

    #[tokio::test]
    async fn aio_never_stops_or_restarts_consumers_and_releases_lock_on_all_returns() {
        for outcome in [
            BackupOutcome::Success,
            BackupOutcome::Failure,
            BackupOutcome::Cancelled,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let ops = fake_operations(dir.path(), true, outcome);
            let markers = MaintenanceMarkerConfig::new(&ops.markers, StdDuration::from_secs(3600));
            let result = backup_volume(&ops, &aio_volume(), Some(&markers), &Shutdown::new()).await;
            assert_eq!(result.is_ok(), matches!(outcome, BackupOutcome::Success));
            assert_eq!(
                *ops.events.borrow(),
                ["inspect:custom-borg", "inspect:custom-dump", "backup"]
            );
            assert!(!ops.borg_dir.join("aio-lockfile").exists());
            assert!(!ops.markers.exists());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_aio_never_starts_restic_or_touches_containers() {
        let dir = tempfile::tempdir().unwrap();
        let ops = fake_operations(dir.path(), true, BackupOutcome::Success);
        fs::write(ops.dump_dir.join("backup-is-running"), "").unwrap();
        let result = backup_volume(&ops, &aio_volume(), None, &Shutdown::new()).await;
        assert!(matches!(result, Err(Error::AioTimeout { .. })));
        assert_eq!(
            *ops.events.borrow(),
            ["inspect:custom-borg", "inspect:custom-dump"]
        );
        assert!(!ops.borg_dir.join("aio-lockfile").exists());
    }

    #[tokio::test]
    async fn stop_consumers_restarts_and_cleans_markers_after_success_failure_or_cancellation() {
        for outcome in [
            BackupOutcome::Success,
            BackupOutcome::Failure,
            BackupOutcome::Cancelled,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let ops = fake_operations(dir.path(), false, outcome);
            let markers = MaintenanceMarkerConfig::new(&ops.markers, StdDuration::from_secs(3600));
            let config = VolumeBackupConfig {
                name: "ordinary".into(),
                consistency: BackupConsistency::StopConsumers,
            };
            let result = backup_volume(&ops, &config, Some(&markers), &Shutdown::new()).await;
            assert_eq!(result.is_ok(), matches!(outcome, BackupOutcome::Success));
            assert_eq!(
                *ops.events.borrow(),
                ["inspect:ordinary", "consumers", "stop", "backup", "start"]
            );
            assert!(!ops.markers.join("my-app.json").exists());
        }
    }

    #[tokio::test]
    async fn shutdown_kills_and_reaps_child_before_aio_lock_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("aio-lockfile");
        let pid_path = dir.path().join("child.pid");
        let shutdown = Shutdown::new();
        let guard = crate::aio::AioLockGuard::try_acquire(&lock_path).unwrap();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf '%s' \"$$\" > \"$1\"; exec sleep 3600")
            .arg("test-child")
            .arg(&pid_path);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let backup = async {
            let result = run_command(command, &shutdown).await;
            assert!(matches!(result, Err(Error::Cancelled)));
            assert!(lock_path.exists(), "lock must cover child termination");
            let pid = fs::read_to_string(&pid_path).unwrap();
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "child must be reaped before releasing lock"
            );
            drop(guard);
        };
        let cancel = async {
            loop {
                if fs::read_to_string(&pid_path).is_ok_and(|pid| !pid.is_empty()) {
                    break;
                }
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
            assert!(lock_path.exists());
            shutdown.request();
        };
        tokio::time::timeout(StdDuration::from_secs(5), async {
            tokio::join!(backup, cancel);
        })
        .await
        .unwrap();
        assert!(!lock_path.exists());
    }

    #[tokio::test]
    async fn command_output_drains_both_pipes_and_preserves_exit_status() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf 'stdout'; printf 'stderr' >&2; exit 7");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = run_command(command, &Shutdown::new()).await.unwrap();
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");
        assert_eq!(output.status.code(), Some(7));
    }
}
