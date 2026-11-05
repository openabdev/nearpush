# Build context is the repository root:
#   docker buildx build --platform linux/amd64 -f deploy/eventrelay/Dockerfile -t eventrelay:0.1.0 --load .
# Only the workspace manifests and crates are copied, so the build does not depend on
# local target/ or .agents/ contents. The .dockerignore next to this file trims the context.

FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p eventrelay --bin eventrelay \
 && install -d -o 65532 -g 65532 -m 0700 /data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/eventrelay /usr/local/bin/eventrelay
COPY --from=build --chown=65532:65532 /data /data
ENV EVENTRELAY_DB=/data/eventrelay.db
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/eventrelay"]
