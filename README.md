# nerd-backup

[![Build and Publish Docker Image](https://github.com/avranju/nerd-backup/actions/workflows/docker_build_and_publish.yml/badge.svg)](https://github.com/avranju/nerd-backup/actions/workflows/docker_build_and_publish.yml)

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
NERD_BACKUP_BACKUP_INTERVAL=PT24H
NERD_BACKUP_SNAPSHOT_RETENTION=P3D
NERD_BACKUP_MAINTENANCE_MARKER_DIR=/run/nerd-watch/maintenance
NERD_BACKUP_MAINTENANCE_MARKER_TTL=PT1H
```

- `NERD_BACKUP_RESTIC_REPOSITORY`: Full path to the Restic repository (e.g., `s3:s3.ap-south-1.amazonaws.com/nerdworks-backup/vm1`).
- `NERD_BACKUP_RESTIC_PASSWORD`: Restic repository password. **Crucial for restores; keep secure.**
- `NERD_BACKUP_AWS_ACCESS_KEY_ID`: Your AWS Access Key ID.
- `NERD_BACKUP_AWS_SECRET_ACCESS_KEY`: Your AWS Secret Access Key. **Do not share.**
- `NERD_BACKUP_VOLUMES_TO_BACKUP`: Comma-separated list of Docker volume names to back up (e.g., `my_app_data,db_data`).
- `NERD_BACKUP_TAG_PREFIX`: Prefix for Restic snapshot tags (e.g., `daily-`).
- `NERD_BACKUP_BACKUP_INTERVAL`: Interval at which backups should be taken specified in ISO 8601 format.
- `NERD_BACKUP_SNAPSHOT_RETENTION` (Optional): Duration in ISO 8601 format specifying how long to retain snapshots. Older snapshots will be pruned automatically (e.g., `P3D` for 3 days, `P1W` for 1 week, `P1M` for 1 month). If not specified, no automatic pruning occurs.
- `NERD_BACKUP_MAINTENANCE_MARKER_DIR` (Optional): Directory shared with [`nerd-watch`](https://github.com/avranju/nerd-watch) for per-container maintenance markers. When set, `nerd-backup` writes `<container>.json` before stopping a container and deletes it after that container's backup flow completes.
- `NERD_BACKUP_MAINTENANCE_MARKER_TTL` (Optional): ISO 8601 duration used for marker expiration. Defaults to `PT1H` when `NERD_BACKUP_MAINTENANCE_MARKER_DIR` is configured.

### ISO 8601 Duration Format Examples

For both `NERD_BACKUP_BACKUP_INTERVAL` and `NERD_BACKUP_SNAPSHOT_RETENTION`:

- `PT1H` - 1 hour
- `PT12H` - 12 hours  
- `P1D` - 1 day
- `P3D` - 3 days
- `P1W` - 1 week
- `P2W` - 2 weeks
- `P1M` - 1 month
- `P3M` - 3 months
- `P1Y` - 1 year

Execute the compiled application:

```bash
./target/release/nerd-backup
```

Upon execution, the application will:

1.  Load configuration from `.env`.
2.  Initialize the Restic repository on S3 (if not present).
3.  Back up specified Docker volumes to the Restic repository on S3.
4.  Prune old snapshots based on the retention policy (if configured).
5.  Output progress and status to the console.

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
  -e NERD_BACKUP_BACKUP_INTERVAL="PT24H" \
  -e NERD_BACKUP_SNAPSHOT_RETENTION="P3D" \
  -e NERD_BACKUP_MAINTENANCE_MARKER_DIR="/run/nerd-watch/maintenance" \
  -e NERD_BACKUP_MAINTENANCE_MARKER_TTL="PT1H" \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  -v /var/lib/docker/volumes:/var/lib/docker/volumes:ro \
  -v /run/nerd-watch/maintenance:/run/nerd-watch/maintenance \
  -v nerd-backup-data:/var/lib/nerd-backup \
  nerd-backup
```

Replace `<placeholder>` values with your specific configuration.

When using [`nerd-watch`](https://github.com/avranju/nerd-watch), mount the same host directory into both containers and set `NERD_WATCH_MAINTENANCE_DIR` on [`nerd-watch`](https://github.com/avranju/nerd-watch) to that path. For example, mount `/run/nerd-watch/maintenance` into `nerd-backup` as shown above and configure `NERD_WATCH_MAINTENANCE_DIR=/run/nerd-watch/maintenance` for [`nerd-watch`](https://github.com/avranju/nerd-watch).

### Running with Docker Compose (One-Off Job)

Use the provided `docker-compose.yml` to run `nerd-backup` as a one-off job. First, update the environment variable placeholders in `docker-compose.yml`.

```yaml
services:
  nerd-backup:
    image: nerd-backup:latest
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - /var/lib/docker/volumes:/var/lib/docker/volumes:ro
      - /run/nerd-watch/maintenance:/run/nerd-watch/maintenance
      - nerd-backup-data:/var/lib/nerd-backup
    environment:
      - NERD_BACKUP_RESTIC_REPOSITORY=<your_restic_repository_path>
      - NERD_BACKUP_RESTIC_PASSWORD=<your_restic_password>
      - NERD_BACKUP_AWS_ACCESS_KEY_ID=<your_aws_access_key_id>
      - NERD_BACKUP_AWS_SECRET_ACCESS_KEY=<your_aws_secret_access_key>
      - NERD_BACKUP_VOLUMES_TO_BACKUP=<volume1,volume2,volume3>
      - NERD_BACKUP_TAG_PREFIX=<your_tag_prefix>
      - NERD_BACKUP_BACKUP_INTERVAL=PT24H
      - NERD_BACKUP_SNAPSHOT_RETENTION=P3D
      - NERD_BACKUP_MAINTENANCE_MARKER_DIR=/run/nerd-watch/maintenance
      - NERD_BACKUP_MAINTENANCE_MARKER_TTL=PT1H
    restart: "no"

volumes:
  nerd-backup-data:
```

From the project root, execute:

```bash
docker compose up --build nerd-backup
```

This command will build (if necessary) and run the `nerd-backup` service, executing the backup, and then stopping the container upon completion.
