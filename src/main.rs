use anyhow::{Ok, Result};
use humantime::format_duration;
use iso8601::Duration;
use serde::Deserialize;
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

mod error;
mod restic;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub restic_repository: String,
    pub restic_password: String,
    pub aws_access_key_id: String,
    pub aws_secret_access_key: String,
    pub volumes_to_backup: Vec<String>,
    pub tag_prefix: String,
    pub backup_interval: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    // Initialize tracing subscriber for logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");

    // Check if /var/lib/nerd-backup exists, create if it doesn't
    let backup_dir = "/var/lib/nerd-backup";
    if !Path::new(backup_dir).exists() {
        fs::create_dir_all(backup_dir)?;
        tracing::info!("Created directory: {}", backup_dir);
    } else {
        tracing::info!("Directory already exists: {}", backup_dir);
    }

    let config = envy::prefixed("NERD_BACKUP_").from_env::<Config>()?;

    // Parse the backup interval
    let duration = parse_iso8601_duration(&config.backup_interval)?;
    tracing::info!("Backup interval set to: {}", format_duration(duration));

    let backend = restic::Backend::S3 {
        access_key_id: config.aws_access_key_id,
        secret_access_key: config.aws_secret_access_key,
    };
    let restic = restic::Restic::new(
        config.restic_repository,
        config.restic_password,
        backend,
        config.tag_prefix,
    );
    restic.init().await?;

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
                    sleep(TokioDuration::from_secs(remaining)).await;
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

    // Create a future that resolves when a shutdown signal is received
    let mut sigterm = unix::signal(unix::SignalKind::terminate())?;
    let mut sigint = unix::signal(unix::SignalKind::interrupt())?;

    // Run backup in a loop
    loop {
        select! {
            _ = interval_timer.tick() => {
                tracing::info!("Starting backup.");

                // Run the backup
                match restic.backup(config.volumes_to_backup.clone()).await {
                    StdOk(_) => {
                        tracing::info!("Backup completed successfully");
                        // Update the last run timestamp only on success
                        if let Err(e) = update_last_run_timestamp(&last_run_file) {
                            tracing::error!("Failed to update last run timestamp: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Backup failed: {}", e);
                        // Don't update the last run timestamp on failure
                        // Continue to the next iteration to wait for the next scheduled interval
                    }
                }
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down gracefully");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, shutting down gracefully");
                break;
            }
        }
    }

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
        .map_err(|e| anyhow::anyhow!("Failed to parse duration: {:?}", e))?;
    Ok(duration.into())
}
