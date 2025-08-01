FROM rust:1.88-slim AS builder

WORKDIR /usr/src/app
COPY compiler ./compiler
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release


FROM quay.io/podman/stable AS wasm-compiler

USER podman
WORKDIR /home/podman

COPY --from=builder /usr/src/app/target/release/service-compiler /usr/bin/service-compiler
COPY compiler ./compiler
COPY entrypoint.sh /usr/local/bin/entrypoint.sh


ENTRYPOINT ["/bin/bash", "/usr/local/bin/entrypoint.sh"]
CMD ["service-compiler"]