# Builds the `lan-probe` binary for scripts/lan-two-containers.sh.
# Nightly toolchain per rust-toolchain.toml; no default features, so the Tauri
# layer (and its GTK / D-Bus system libraries) is left out.
FROM rustlang/rust:nightly-bookworm AS build
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends cmake pkg-config >/dev/null && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps
RUN cargo build --release -p xdb --no-default-features --bin lan-probe

FROM debian:bookworm-slim
COPY --from=build /src/target/release/lan-probe /usr/local/bin/lan-probe
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/lan-probe"]
