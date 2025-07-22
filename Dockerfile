# Stage 1: Build the Rust application
FROM rust:slim-bookworm AS builder

WORKDIR /app

# Copy Rust project files
COPY Cargo.toml Cargo.lock ./
COPY src ./src/

# Build the release binary
RUN cargo build --release

# Stage 2: Copy restic binary
FROM restic/restic:0.18.0 AS restic

# Stage 3: Create the final image
FROM debian:bookworm-slim

# Install root certificates
RUN apt-get update
RUN apt install -y ca-certificates

WORKDIR /app

# Copy the restic binary from the restic stage
COPY --from=restic /usr/bin/restic /usr/bin/restic

# Copy the compiled executable from the builder stage
COPY --from=builder /app/target/release/nerd-backup /app/nerd-backup

# Set the entrypoint to run the application
ENTRYPOINT ["/app/nerd-backup"]
