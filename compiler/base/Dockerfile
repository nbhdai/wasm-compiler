#syntax=docker/dockerfile:1.5
FROM ubuntu:24.04 as toolchain
ARG channel
ENV DEBIAN_FRONTEND="noninteractive"
# `build-essential` and `file` are needed for backtrace-sys

# `cmake`, `git`, `python` are needed for wasm tools

RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    curl \
    file \
    gcc \
    git \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m wasmbuild -d /wasmbuild

RUN usermod -p '!!' root # Disable all passwords for root
# Attach the security note

COPY --chown=wasmbuild attach_notice.sh security_notice.txt /wasmbuild/
RUN /wasmbuild/attach_notice.sh /wasmbuild/security_notice.txt /etc/passwd && \
    /wasmbuild/attach_notice.sh /wasmbuild/security_notice.txt /etc/shadow && \
    rm -f /wasmbuild/attach_notice.sh
USER wasmbuild
ENV USER=wasmbuild
ENV PATH=/wasmbuild/.cargo/bin:$PATH
WORKDIR /wasmbuild
# Ensure that we are using the latest stable version of rustup and the
# latest version of the current channel. A new manifest will trigger
# these lines to run again, forcing a new download of rustup and
# installation of Rust.

ADD --chown=wasmbuild https://static.rust-lang.org/rustup/release-stable.toml /wasmbuild/tools/rustup-manifest.toml
ADD --chown=wasmbuild https://static.rust-lang.org/dist/channel-rust-${channel}-date.txt /wasmbuild/tools/rust-channel-version
RUN curl https://sh.rustup.rs -sSf | sh -s -- \
    -y \
    --profile minimal \
    --default-toolchain "${channel}" \
    --target wasm32-unknown-unknown \
    --component rustfmt \
    --component clippy \
    --component rust-src

RUN if [ "${channel}" = 'nightly' ]; then rustup component add miri; fi
COPY --chown=wasmbuild entrypoint.sh /wasmbuild/tools/

# Build wasm-tools
FROM toolchain as wasm-tools

RUN cargo install --locked wasm-tools
# Fetch all the crate source files
FROM toolchain as sources

RUN cargo init /wasmbuild
COPY --chown=wasmbuild lib.rs /wasmbuild/src/lib.rs
COPY --chown=wasmbuild Cargo.toml /wasmbuild/Cargo.toml
COPY --chown=wasmbuild crate-information.json /wasmbuild/crate-information.json
RUN cargo build --target=wasm32-unknown-unknown
RUN cargo build --target=wasm32-unknown-unknown --release
RUN rm src/*.rs

# Set up cargo-chef for faster builds
FROM toolchain as chef-available
RUN cargo install --locked cargo-chef
WORKDIR /orchestrator

# Prepare the orchestrator's dependencies
FROM chef-available as prepare-orchestrator
COPY --chown=wasmbuild asm-cleanup /asm-cleanup
COPY --chown=wasmbuild modify-cargo-toml /modify-cargo-toml
COPY --chown=wasmbuild orchestrator /orchestrator

RUN cargo chef prepare

# Build the orchestrator
FROM chef-available as build-orchestrator

COPY --chown=wasmbuild asm-cleanup /asm-cleanup
COPY --chown=wasmbuild modify-cargo-toml /modify-cargo-toml
COPY --chown=wasmbuild --from=prepare-orchestrator /orchestrator/recipe.json /orchestrator/recipe.json
RUN cargo chef cook --locked --release
COPY --chown=wasmbuild orchestrator /orchestrator
RUN cargo install --locked --path .

# Compiler and pre-compiled crates
FROM sources 
ARG channel

COPY lib.rs src/lib.rs
RUN cargo build --target=wasm32-unknown-unknown
RUN cargo build --target=wasm32-unknown-unknown --release
RUN rm src/*.rs

COPY clean_up.sh .
RUN ./clean_up.sh

COPY --from=build-orchestrator /wasmbuild/.cargo/bin/worker /wasmbuild/.cargo/bin/worker
COPY --from=wasm-tools /wasmbuild/.cargo/bin/wasm-tools /wasmbuild/.cargo/bin
COPY --chown=wasmbuild cargo-wasm /wasmbuild/.cargo/bin
ENTRYPOINT ["/wasmbuild/tools/entrypoint.sh"]