# nerd-backup

`nerd-backup` is a Rust application for backing up Docker volumes to an Amazon S3 bucket using [Restic](https://github.com/restic/restic).

## Overview

Here's what it does:

- **Docker Volume Backup:** Backs up specified Docker volumes.
- **Restic Integration:** Utilizes Restic for secure, efficient, and deduplicated backups.
- **S3 Backend:** Stores backups in an Amazon S3 bucket.
- **Environment Variable Configuration:** Configures all settings via environment variables.

## Usage Guide

### Prerequisites

Ensure the following are installed and configured:

- **Rust and Cargo:** Install from the [official Rust website](https://www.rust-lang.org/tools/install).
- **Docker:** You already have persistent services storing data in docker volumes that you want backed up.
- **Restic:** `nerd-backup` integrates with Restic internally.
- **AWS S3 bucket:** A bucket for backup storage.
- **AWS credentials:** An AWS Access Key ID and Secret Access Key with S3 read/write permissions.

### Building the Project

1.  **Clone the repository:**

    ```bash
    git clone https://github.com/avranju/nerd-backup.git
    cd nerd-backup
    ```

2.  **Build the application:**
    ```bash
    cargo build --release
    ```
    The executable will be generated in `target/release/`.

### Running the Application (Direct Execution)

Create a `.env` file in the project root directory with the following environment variables. Replace placeholders with your specific values.

```ini
NERD_BACKUP_RESTIC_REPOSITORY=<your_restic_repository_path>
NERD_BACKUP_RESTIC_PASSWORD=<your_restic_password>
NERD_BACKUP_AWS_ACCESS_KEY_ID=<your_aws_access_key_id>
NERD_BACKUP_AWS_SECRET_ACCESS_KEY=<your_aws_secret_access_key>
NERD_BACKUP_VOLUMES_TO_BACKUP=<volume1,volume2,volume3>
NERD_BACKUP_TAG_PREFIX=<your_tag_prefix>
```

- `NERD_BACKUP_RESTIC_REPOSITORY`: Full path to the Restic repository (e.g., `s3:s3.ap-south-1.amazonaws.com/nerdworks-backup/vm1`).
- `NERD_BACKUP_RESTIC_PASSWORD`: Restic repository password. **Crucial for restores; keep secure.**
- `NERD_BACKUP_AWS_ACCESS_KEY_ID`: Your AWS Access Key ID.
- `NERD_BACKUP_AWS_SECRET_ACCESS_KEY`: Your AWS Secret Access Key. **Do not share.**
- `NERD_BACKUP_VOLUMES_TO_BACKUP`: Comma-separated list of Docker volume names to back up (e.g., `my_app_data,db_data`).
- `NERD_BACKUP_TAG_PREFIX`: Prefix for Restic snapshot tags (e.g., `daily-`).

Execute the compiled application:

```bash
./target/release/nerd-backup
```

Upon execution, the application will:

1.  Load configuration from `.env`.
2.  Initialize the Restic repository on S3 (if not present).
3.  Back up specified Docker volumes to the Restic repository on S3.
4.  Output progress and status to the console.

Note that Docker volumes are typically stored with only `root` user access on the file system. If running as a non-root user, the backup may fail due to insufficient permissions to access volume files. Running as `sudo` is often required.

## Docker Usage

Alternatively, `nerd-backup` can be run within a Docker container. A `Dockerfile` and `docker-compose.yml` are provided for this purpose.

### Building the Docker Image

From the project root, build the Docker image:

```bash
docker build -t nerd-backup .
```

### Running with Docker (Direct Container Execution)

Run the built Docker image as a container, providing environment variables directly:

```bash
docker run --rm \
  -e NERD_BACKUP_RESTIC_REPOSITORY="<your_restic_repository_path>" \
  -e NERD_BACKUP_RESTIC_PASSWORD="<your_restic_password>" \
  -e NERD_BACKUP_AWS_ACCESS_KEY_ID="<your_aws_access_key_id>" \
  -e NERD_BACKUP_AWS_SECRET_ACCESS_KEY="<your_aws_secret_access_key>" \
  -e NERD_BACKUP_VOLUMES_TO_BACKUP="<volume1,volume2>" \
  -e NERD_BACKUP_TAG_PREFIX="<your_tag_prefix>" \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  -v /var/lib/docker/volumes:/var/lib/docker/volumes:ro \
  nerd-backup
```

Replace `<placeholder>` values with your specific configuration.

### Running with Docker Compose (One-Off Job)

Use the provided `docker-compose.yml` to run `nerd-backup` as a one-off job. First, update the environment variable placeholders in `docker-compose.yml`.

```yaml
version: '3.8'

services:
  nerd-backup:
    image: nerd-backup:latest
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - /var/lib/docker/volumes:/var/lib/docker/volumes:ro
    environment:
      - NERD_BACKUP_RESTIC_REPOSITORY=<your_restic_repository_path>
      - NERD_BACKUP_RESTIC_PASSWORD=<your_restic_password>
      - NERD_BACKUP_AWS_ACCESS_KEY_ID=<your_aws_access_key_id>
      - NERD_BACKUP_AWS_SECRET_ACCESS_KEY=<your_aws_secret_access_key>
      - NERD_BACKUP_VOLUMES_TO_BACKUP=<volume1,volume2,volume3>
      - NERD_BACKUP_TAG_PREFIX=<your_tag_prefix>
    restart: "no"
```

From the project root, execute:

```bash
docker compose up --build nerd-backup
```

This command will build (if necessary) and run the `nerd-backup` service, executing the backup, and then stopping the container upon completion.
