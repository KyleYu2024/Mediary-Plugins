FROM rust:1-bookworm

WORKDIR /app/plugins

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY maoyan-rank ./maoyan-rank
COPY pansou ./pansou

RUN cargo build --locked --release \
    && mkdir -p /opt/plugins/maoyan-rank /opt/plugins/pansou \
    && cp target/release/maoyan-rank-plugin /opt/plugins/maoyan-rank/plugin \
    && cp target/release/pansou-plugin /opt/plugins/pansou/plugin \
    && cp maoyan-rank/plugin.json /opt/plugins/maoyan-rank/plugin.json \
    && cp pansou/plugin.json /opt/plugins/pansou/plugin.json
