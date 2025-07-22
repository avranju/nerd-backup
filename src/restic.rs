use std::{collections::HashMap, process::Stdio};

use bollard::query_parameters::{
    ListContainersOptions, StartContainerOptions, StopContainerOptions,
};
use tokio::process::Command;

use crate::error::{CheckError, Error};

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
}

impl Restic {
    pub fn new(repository: String, password: String, backend: Backend, tag_prefix: String) -> Self {
        Restic {
            repository,
            password,
            backend,
            tag_prefix,
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
        let docker = bollard::Docker::connect_with_socket_defaults()?;

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

            // Stop all containers using this volume
            stop_containers(&docker, &containers).await?;

            let res = self.do_backup(vol_info).await;

            // Start all containers again
            start_containers(&docker, &containers).await?;

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
