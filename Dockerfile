
# Stage 1: Build the Rust application
FROM rust:slim-bookworm AS builder

WORKDIR /app

# Copy Rust project files
COPY Cargo.toml Cargo.lock ./
COPY src ./src/

# Build the release binary
RUN cargo build --release

# Stage 2: Create the final image
FROM restic/restic:0.18.0

WORKDIR /app

# Copy the compiled executable from the builder stage
COPY --from=builder /app/target/release/nerd-backup ./nerd-backup

# Set the entrypoint to run the application
ENTRYPOINT ["./nerd-backup"]
