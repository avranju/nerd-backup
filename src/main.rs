use anyhow::{Ok, Result};
use humantime::format_duration;
use iso8601::Duration;
use std::{
    fs,
    io::Write,
    path::Path,
    result::Result::Ok as StdOk,
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    select,
    signal::unix,
    time::{Duration as TokioDuration, interval, sleep},
};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

mod aio;
mod config;
mod error;
mod restic;
mod secret;
mod shutdown;

use config::Config;
use shutdown::Shutdown;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    // Initialize tracing subscriber for logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");

    let config = Config::load()?;
    let shutdown = Shutdown::new();
    // Register signals before repository initialization or the initial scheduled wait.
    let mut sigterm = unix::signal(unix::SignalKind::terminate())?;
    let mut sigint = unix::signal(unix::SignalKind::interrupt())?;
    let signal_shutdown = shutdown.clone();
    let signal_task = tokio::spawn(async move {
        select! {
            _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down gracefully"),
            _ = sigint.recv() => tracing::info!("Received SIGINT, shutting down gracefully"),
        }
        signal_shutdown.request();
    });

    // Check if /var/lib/nerd-backup exists, create if it doesn't
    let backup_dir = "/var/lib/nerd-backup";
    if !Path::new(backup_dir).exists() {
        fs::create_dir_all(backup_dir)?;
        tracing::info!("Created directory: {}", backup_dir);
    } else {
        tracing::info!("Directory already exists: {}", backup_dir);
    }

    // Parse the backup interval and Docker API timeout.
    let duration = parse_iso8601_duration(&config.backup.interval)?;
    tracing::info!("Backup interval set to: {}", format_duration(duration));
    let docker_api_timeout = parse_docker_api_timeout(config.backup.docker_api_timeout.as_deref())?;
    tracing::info!(
        "Docker API timeout set to: {}",
        format_duration(docker_api_timeout)
    );

    let maintenance_markers = match &config.maintenance {
        Some(maintenance) => {
            let ttl = match &maintenance.marker_ttl {
                Some(ttl) => parse_iso8601_duration(ttl)?,
                None => StdDuration::from_secs(60 * 60),
            };
            Some(restic::MaintenanceMarkerConfig::new(
                &maintenance.marker_dir,
                ttl,
            ))
        }
        None => None,
    };

    let backend = restic::Backend::S3 {
        access_key_id: config.aws.access_key_id,
        secret_access_key: config.aws.secret_access_key,
    };
    let restic = restic::Restic::new(
        config.restic,
        backend,
        docker_api_timeout,
        maintenance_markers,
        shutdown.clone(),
    );
    match restic.init().await {
        Err(error::Error::Cancelled) => return Ok(()),
        result => result?,
    }

    // Check when the last backup was run
    let last_run_file = format!("{}/last-run", backup_dir);

    if Path::new(&last_run_file).exists() {
        // Read the last run timestamp
        let last_run_content = fs::read_to_string(&last_run_file)?;
        let parse_result = last_run_content.trim().parse::<u64>();
        match parse_result {
            StdOk(last_run_timestamp) => {
                let current_timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

                let elapsed = current_timestamp - last_run_timestamp;

                if elapsed < duration.as_secs() {
                    // Calculate remaining time until next backup
                    let remaining = duration.as_secs() - elapsed;
                    tracing::info!(
                        "Waiting {} seconds until next scheduled backup",
                        format_duration(StdDuration::from_secs(remaining))
                    );
                    select! {
                        biased;
                        _ = shutdown.cancelled() => return Ok(()),
                        _ = sleep(TokioDuration::from_secs(remaining)) => {},
                    }
                } else {
                    tracing::info!(
                        "Backup is overdue by {} seconds, running immediately",
                        elapsed - duration.as_secs()
                    );
                }
            }
            Err(_) => {
                tracing::warn!("Failed to parse last run timestamp, running backup immediately");
            }
        }
    } else {
        tracing::info!("No previous backup found, running backup immediately");
    }

    // Create a timer that ticks at the specified interval
    let mut interval_timer = interval(duration);

    // Run backup in a loop
    loop {
        select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval_timer.tick() => {
                tracing::info!("Starting backup.");

                // Run the backup. Snapshot retention is independent of the aggregate
                // backup result: a failure in one volume must not prevent cleanup of
                // snapshots created by earlier volumes or previous runs.
                let backup_result = restic.backup(&config.volumes).await;

                // Normal backup failures still prune. Shutdown does not start new
                // maintenance work; cancelled children are already killed/reaped.
                if !shutdown.is_requested()
                    && let Err(e) = restic.prune_snapshots().await {
                    tracing::error!("Failed to prune old snapshots: {}", e);
                }

                match backup_result {
                    StdOk(_) => {
                        tracing::info!("Backup completed successfully");

                        // Update the last run timestamp only on success.
                        if let Err(e) = update_last_run_timestamp(&last_run_file) {
                            tracing::error!("Failed to update last run timestamp: {}", e);
                        }
                    }
                    Err(error::Error::Cancelled) => break,
                    Err(e) => {
                        tracing::error!("Backup failed: {}", e);
                        // Don't update the last run timestamp on failure.
                        // Continue to the next iteration to wait for the next scheduled interval.
                    }
                }
            }
        }
    }

    signal_task.abort();
    tracing::info!("Backup service stopped");
    Ok(())
}

fn update_last_run_timestamp(file_path: &str) -> Result<()> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let mut file = fs::File::create(file_path)?;
    file.write_all(timestamp.to_string().as_bytes())?;
    Ok(())
}

fn parse_iso8601_duration(duration_str: &str) -> Result<TokioDuration> {
    let duration = duration_str
        .parse::<Duration>()
        .map_err(|_| anyhow::anyhow!("Invalid ISO 8601 duration (value omitted)"))?;
    Ok(duration.into())
}

fn parse_docker_api_timeout(timeout: Option<&str>) -> Result<StdDuration> {
    // Operators can raise the default for containers that need longer graceful shutdowns.
    const DEFAULT_TIMEOUT: StdDuration = StdDuration::from_secs(35 * 60);

    let timeout = match timeout {
        Some(timeout) => parse_iso8601_duration(timeout)?,
        None => DEFAULT_TIMEOUT,
    };

    if timeout.is_zero() {
        anyhow::bail!("Docker API timeout must be greater than zero");
    }

    Ok(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_api_timeout_defaults_to_35_minutes() {
        assert_eq!(
            parse_docker_api_timeout(None).unwrap(),
            StdDuration::from_secs(35 * 60)
        );
    }

    #[test]
    fn docker_api_timeout_accepts_iso8601_duration() {
        assert_eq!(
            parse_docker_api_timeout(Some("PT45M")).unwrap(),
            StdDuration::from_secs(45 * 60)
        );
    }

    #[test]
    fn docker_api_timeout_rejects_zero() {
        assert!(parse_docker_api_timeout(Some("PT0S")).is_err());
    }
}
