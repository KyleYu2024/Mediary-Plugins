FROM rust:1-bookworm AS builder

WORKDIR /app/plugins

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY maoyan-rank ./maoyan-rank
COPY pansou ./pansou
COPY telegram-resource ./telegram-resource

RUN cargo build --locked --release \
    && mkdir -p /opt/plugins/maoyan-rank /opt/plugins/pansou /opt/plugins/telegram-resource \
    && cp target/release/maoyan-rank-plugin /opt/plugins/maoyan-rank/plugin \
    && cp target/release/pansou-plugin /opt/plugins/pansou/plugin \
    && cp target/release/telegram-resource-plugin /opt/plugins/telegram-resource/plugin \
    && cp maoyan-rank/plugin.json /opt/plugins/maoyan-rank/plugin.json \
    && cp pansou/plugin.json /opt/plugins/pansou/plugin.json \
    && cp telegram-resource/plugin.json /opt/plugins/telegram-resource/plugin.json

FROM scratch AS telegram-resource-artifact

COPY --from=builder /opt/plugins/telegram-resource/ /
