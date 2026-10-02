# nerd-backup

[![Build and Publish Docker Image](https://github.com/avranju/nerd-backup/actions/workflows/docker_build_and_publish.yml/badge.svg)](https://github.com/avranju/nerd-backup/actions/workflows/docker_build_and_publish.yml)

`nerd-backup` backs up Docker volumes to an Amazon S3 repository using [Restic](https://github.com/restic/restic). Each volume selects a consistency strategy. The service runs immediately when a backup is due, then repeats at the configured interval. `/var/lib/nerd-backup/last-run` records the last fully successful run.

## Configuration

Copy [config.example.toml](config.example.toml) to `/etc/nerd-backup/config.toml`, or choose a path with `NERD_BACKUP_CONFIG=/path/to/config.toml`. Keep the file readable only by the service user: it contains credentials and the Restic password needed for restoration.

```toml
[restic]
repository = "s3:s3.ap-south-1.amazonaws.com/example-bucket/server1"
password = "replace-with-restic-password"
tag_prefix = "daily-"
snapshot_retention = "P3D"

[aws]
access_key_id = "replace-with-access-key"
secret_access_key = "replace-with-secret-key"

[backup]
interval = "PT24H"
docker_api_timeout = "PT35M"

[maintenance]
marker_dir = "/run/nerd-watch/maintenance"
marker_ttl = "PT1H"

[[volumes]]
name = "ordinary_volume"
consistency = "stop-consumers"

[[volumes]]
name = "nextcloud_aio_backupdir"
consistency = "aio-borg-lock"

[volumes.aio]
lockfile = "borg/aio-lockfile"
running_marker_volume = "nextcloud_aio_database_dump"
running_marker_path = "backup-is-running"
wait_interval = "PT30S"
wait_timeout = "PT1H"
```

`restic`, `aws`, `backup`, and `volumes` are required. Every volume requires a name and consistency strategy; only `aio-borg-lock` requires the adjacent `[volumes.aio]` table. No AIO fields are needed for ordinary volumes. Unknown strategies and missing AIO fields fail during configuration loading.

- `restic.snapshot_retention` is optional. Omit it to disable pruning. The existing Restic `forget --prune --keep-within` behavior and ISO duration conversion are preserved, including whole-day/hour/minute rounding. Snapshot tags remain `<tag_prefix><volume_name>`.
- `backup.docker_api_timeout` defaults to `PT35M`. Set it above the longest graceful container stop timeout.
- The entire `maintenance` section is optional. `marker_ttl` defaults to `PT1H`; markers apply only to `stop-consumers`.
- `interval`, timeouts, polling interval, and marker TTL use ISO 8601 durations and must be positive. Examples: `PT30S`, `PT1H`, `PT24H`, `P3D`.
- Both AIO paths are relative to their respective Docker volume mountpoints and cannot be empty, absolute, or contain `..`. All AIO fields are required and configurable, including both volume names. The lockfile's parent directory must already exist; nerd-backup does not initialize Borg repositories.

Configuration precedence is:

1. `NERD_BACKUP_CONFIG`, including when set in `.env`. An unreadable, missing, or invalid explicit file is an error.
2. `/etc/nerd-backup/config.toml`, if present. An invalid or unreadable default file, including a dangling symlink, is an error.
3. Legacy `NERD_BACKUP_*` environment variables, only when the default file is absent and no explicit path was set.

There is no merging of TOML fields with environment variables. `.env` loading is still supported, and existing process environment values take precedence over `.env` values. There is no CLI path option.

## Consistency strategies

### stop-consumers

For each ordinary volume, nerd-backup inspects it through Docker, lists all attached containers, creates configured nerd-watch maintenance markers, stops the consumers, runs Restic against the mountpoint, restarts the consumers, and removes its maintenance markers. Restart and marker cleanup still run when Restic fails or is cancelled. Existing container selection and stop/start error semantics are preserved, including starting all listed consumers after backup.

Markers contain an RFC 3339 `expires_at` and the reason `volume backup`. Share `maintenance.marker_dir` with [nerd-watch](https://github.com/avranju/nerd-watch), and set `NERD_WATCH_MAINTENANCE_DIR` there to the same directory. Cleanup errors are logged without replacing an existing backup error. A Docker stop/start error still ends the current run.

### aio-borg-lock

For a local Nextcloud AIO Borg backup directory, nerd-backup uses [AIO's external locking protocol](https://github.com/nextcloud/all-in-one#sync-local-backups-regularly-to-another-drive). It resolves both configured volumes through Docker and then:

1. Waits while `backup-is-running` exists in the database dump volume.
2. Atomically creates `aio-lockfile` with create-if-absent semantics. An existing lock blocks acquisition.
3. Checks `backup-is-running` again. If it appeared, releases its lock and retries.
4. Holds its lock through the Restic backup, then releases it.

AIO volumes using this strategy never have consumers listed, stopped, or started, and never receive nerd-watch maintenance markers. Timeout errors identify the target volume, the observed blocker (`aio-lockfile`, `backup-is-running`, or both), and elapsed wait time. Both Docker mountpoints must be absolute, accessible directories. Missing marker parent directories, permission errors, and other filesystem errors fail the acquisition rather than treating inaccessible signals as absent.

For the usual volume mounted at AIO's `/mnt/borgbackup`, the repository lives in `borg/`, so use `lockfile = "borg/aio-lockfile"`. If your volume mountpoint is the repository itself, use `"aio-lockfile"`. Confirm your installation's layout: [AIO's entrypoint](https://github.com/nextcloud/all-in-one/blob/main/Containers/borgbackup/start.sh) defines the repository location and [backup script](https://github.com/nextcloud/all-in-one/blob/main/Containers/borgbackup/backupscript.sh) checks the external lock. This strategy targets AIO's local Borg repository; AIO does not honor that local external lock for a remote Borg backend.

AIO checks the external lock and creates its running marker in separate operations. The second check handles a race observed during acquisition; this remains an advisory protocol, not a true bidirectional mutex. It does not replace AIO's Borg backup mechanism.

The lock contains nerd-backup ownership metadata (PID and creation time), but **no automatic stale-lock deletion** occurs. Unknown, malformed, and old nerd-backup locks all block until timeout. Normal returns, errors, and graceful SIGTERM/SIGINT shutdown release an acquired lock. On cancellation, Restic is killed and reaped before release. Ordinary consumers are restarted before exit; allow sufficient shutdown grace time for Docker operations.

RAII cannot remove locks after SIGKILL, kernel panic, host crash, power loss, or similar termination without destructors. Inspect surviving locks manually and verify that their owner and any AIO backup/restore have stopped before removing a stale lock. Never blindly remove an unknown lock.

## Nextcloud AIO recovery

With this architecture, **AIO's Borg backup directory should normally be the only AIO volume backed up by nerd-backup**. The database dump volume is inspected for its running marker; it does not need an entry in `volumes`. Configure and schedule AIO's own backups first.

This intentionally creates a **Restic backup of a Borg repository**. Borg produces the coherent Nextcloud/AIO restore point. Restic provides off-site storage and retention of that repository; it does not replace Borg or AIO's restore flow. A successful Restic backup cannot make an absent, outdated, or failed AIO restore point current. Keep AIO's backup encryption password available separately from the Restic password.

The recovery chain is:

```text
Restic/S3
  -> restore nextcloud_aio_backupdir
  -> point AIO at restored Borg repo
  -> use AIO's own restore flow
```

Restore into an offline backup directory, preserving the `borg/` layout and permissions. Restic backs up the whole volume, including the coordination lock present during backup. Before pointing AIO at it, inspect and remove that restored nerd-backup-owned `aio-lockfile` while the restored repository is offline and no operation is using it. Then follow [AIO's restore instructions](https://github.com/nextcloud/all-in-one#how-to-restore-a-backup). Do not independently restore live AIO database/data volumes from different Restic snapshots.

## Legacy environment migration

Existing installations continue to work with the variables below when no TOML file is selected or present. All legacy volumes use `stop-consumers`; AIO consistency requires TOML. See [.env.example](.env.example) for a legacy example.

| Legacy variable (prefix `NERD_BACKUP_`) | TOML field |
| --- | --- |
| `RESTIC_REPOSITORY` | `restic.repository` |
| `RESTIC_PASSWORD` | `restic.password` |
| `AWS_ACCESS_KEY_ID` | `aws.access_key_id` |
| `AWS_SECRET_ACCESS_KEY` | `aws.secret_access_key` |
| `VOLUMES_TO_BACKUP` | One `[[volumes]]` entry per comma-separated name |
| `TAG_PREFIX` | `restic.tag_prefix` |
| `BACKUP_INTERVAL` | `backup.interval` |
| `SNAPSHOT_RETENTION` | `restic.snapshot_retention` (optional) |
| `DOCKER_API_TIMEOUT` | `backup.docker_api_timeout` (optional) |
| `MAINTENANCE_MARKER_DIR` | `maintenance.marker_dir` (optional) |
| `MAINTENANCE_MARKER_TTL` | `maintenance.marker_ttl` (optional) |

After creating the TOML file, remove legacy credential variables from your deployment. Credentials retain their `Secret` redaction in Debug/tracing and subprocess diagnostics. TOML parse errors omit source excerpts to avoid displaying secret values. Protect the file and keep a secure copy of restoration credentials.

## Building and running

Requires Rust/Cargo, access to a Docker daemon and its volume mountpoints, Restic on `PATH`, an S3 bucket, and AWS credentials with the required repository permissions. Docker volume directories often require root access. The service also needs write access to `/var/lib/nerd-backup`, the configured maintenance directory, and each AIO Borg volume to create/remove its lock.

```bash
cargo build --release
NERD_BACKUP_CONFIG=/path/to/config.toml ./target/release/nerd-backup
```

Volumes are processed sequentially. As before, a volume failure ends that backup run, but snapshot pruning still occurs independently of backup success. Only a completely successful backup updates `last-run`. Shutdown skips starting further backup/prune work.

## Docker

```bash
cp config.example.toml config.toml
chmod 600 config.toml
# Edit config.toml with credentials and actual volume names.
docker build -t nerd-backup .
```

The supplied [docker-compose.yml](docker-compose.yml) shows a persistent service using a read-only configuration mount, a read-only Docker volume root, and a writable overlay for the AIO backup volume. For a locally built image, change `image` to `nerd-backup`. Adjust host paths to the actual mountpoints from `docker volume inspect`; Docker may use a different data root or volume driver.

```bash
docker compose up -d
```

For direct Docker execution:

```bash
docker run --rm \
  --stop-timeout 2400 \
  -v "$PWD/config.toml:/etc/nerd-backup/config.toml:ro" \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  -v /var/lib/docker/volumes:/var/lib/docker/volumes:ro \
  -v /var/lib/docker/volumes/nextcloud_aio_backupdir/_data:/var/lib/docker/volumes/nextcloud_aio_backupdir/_data:rw \
  -v /run/nerd-watch/maintenance:/run/nerd-watch/maintenance \
  -v nerd-backup-data:/var/lib/nerd-backup \
  nerd-backup
```

Mountpoints returned by Docker must be visible inside nerd-backup at **the same absolute paths**. Both AIO volumes need to be readable, and the Borg volume must be writable for the lock. A read-only mount of every volume works only for `stop-consumers`. Remove the writable AIO overlay if no AIO strategy is configured. Adjust the shutdown grace period to cover your containers' stop/start times; forced termination can leave stopped consumers and a stale AIO lock.

## Development checks

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests exercise TOML/legacy migration, credential redaction, atomic lock ownership and cleanup, waiting/timeouts using paused Tokio time, acquisition races, per-strategy container/marker behavior, and child termination before lock release. They do not require Docker or S3.
