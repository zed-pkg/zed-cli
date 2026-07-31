# Immutable builder and runtime inputs for DEN-588's OCI ownership contract.
# rust:1.90.0-bookworm index digest
FROM rust:1.90.0-bookworm@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS zed-builder

WORKDIR /src
COPY zed-interfaces ./zed-interfaces
COPY zed-cli ./zed-cli
WORKDIR /src/zed-cli
RUN cargo build --release --locked --bin zed

# node:22.23.1-bookworm-slim index digest
FROM node:22.23.1-bookworm-slim@sha256:6c74791e557ce11fc957704f6d4fe134a7bc8d6f5ca4403205b2966bd488f6b3

COPY --from=zed-builder /src/zed-cli/target/release/zed /usr/local/bin/zed
WORKDIR /work
