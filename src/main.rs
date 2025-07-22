use anyhow::{Ok, Result};
use serde::Deserialize;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    // Initialize tracing subscriber for logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");

    let config = envy::prefixed("NERD_BACKUP_").from_env::<Config>()?;

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

    restic.backup(config.volumes_to_backup).await?;

    Ok(())
}
