FROM rust:1-bookworm AS builder

WORKDIR /app/plugins

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY hdhive ./hdhive
COPY maoyan-rank ./maoyan-rank
COPY pansou ./pansou

RUN --mount=type=secret,id=hdhive_proxy_secret,required=false \
    if [ -f /run/secrets/hdhive_proxy_secret ]; then \
        export HDHIVE_PROXY_SERVER_SECRET="$(cat /run/secrets/hdhive_proxy_secret)"; \
    fi \
    && cargo build --locked --release \
    && mkdir -p /opt/plugins/hdhive /opt/plugins/maoyan-rank /opt/plugins/pansou \
    && cp target/release/hdhive-plugin /opt/plugins/hdhive/plugin \
    && cp target/release/maoyan-rank-plugin /opt/plugins/maoyan-rank/plugin \
    && cp target/release/pansou-plugin /opt/plugins/pansou/plugin \
    && cp hdhive/plugin.json /opt/plugins/hdhive/plugin.json \
    && cp maoyan-rank/plugin.json /opt/plugins/maoyan-rank/plugin.json \
    && cp pansou/plugin.json /opt/plugins/pansou/plugin.json

FROM scratch AS hdhive-artifact

COPY --from=builder /opt/plugins/hdhive/ /
