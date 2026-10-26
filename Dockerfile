FROM rust:1-slim AS builder
WORKDIR /app
# trunk and the WebAssembly target, to build the web UI (fluxoscope).
ADD https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-x86_64-unknown-linux-gnu.tar.gz /tmp/trunk.tar.gz
RUN tar -xzf /tmp/trunk.tar.gz -C /usr/local/bin trunk && rustup target add wasm32-unknown-unknown
COPY . .
RUN cargo build --release --locked -p fluxolap-server -p fluxoterm \
 && cargo run --release --locked -p xtask -- examples /app/examples-db \
 && rm -f /app/examples-db/registry.bin.zst \
 && cd crates/fluxoscope && trunk build --release --cargo-profile wasm-release --dist /app/ui

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/fluxolap /usr/local/bin/fluxolap
# The terminal viewer/editor: docker exec -it <container> fluxoterm
COPY --from=builder /app/target/release/fluxoterm /usr/local/bin/fluxoterm
# The example databases, imported at startup; run with
# -e FLUXOLAP_EXAMPLES= to start without them.
COPY --from=builder /app/examples-db /usr/share/fluxolap/examples
COPY examples/README.md /usr/share/fluxolap/examples/README.md
# Licenses of the Swagger UI embedded in the binary (the API docs page).
COPY crates/fluxolap-api/assets/swagger-ui/LICENSE crates/fluxolap-api/assets/swagger-ui/NOTICE crates/fluxolap-api/assets/swagger-ui/swagger-ui-bundle.js.LICENSE.txt /usr/share/fluxolap/licenses/swagger-ui/
ENV FLUXOLAP_EXAMPLES=/usr/share/fluxolap/examples
# The web UI (fluxoscope), served at /ui.
COPY --from=builder /app/ui /usr/share/fluxolap/ui
ENV FLUXOLAP_UI_DIR=/usr/share/fluxolap/ui
EXPOSE 7777
ENTRYPOINT ["/usr/local/bin/fluxolap"]
