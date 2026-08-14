FROM rust:bookworm AS zed-builder

WORKDIR /src
COPY zed-interfaces ./zed-interfaces
COPY zed-cli ./zed-cli
WORKDIR /src/zed-cli
RUN cargo build --release --locked --bins

FROM node:22-bookworm-slim

# GitHub-hosted Ubuntu runners use uid/gid 1001. Run this validation image as
# the same non-root identity so files and advisory-lock directories created in
# bind mounts remain accessible to the host-side recovery checks. This keeps
# the production lock permissions intact instead of weakening or chmod'ing
# state after a root container has created it.
RUN groupadd --gid 1001 zed-test \
    && useradd --uid 1001 --gid 1001 --create-home --shell /bin/sh zed-test \
    && mkdir -p /work \
    && chown 1001:1001 /work

COPY --from=zed-builder --chown=1001:1001 /src/zed-cli/target/release/zed /usr/local/bin/zed
COPY --from=zed-builder --chown=1001:1001 /src/zed-cli/target/release/zed-binary /usr/local/bin/zed-binary
ENV HOME=/home/zed-test
USER 1001:1001
WORKDIR /work
