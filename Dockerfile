FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
RUN cargo install writestead --locked --force

FROM node:22-alpine
RUN apk add --no-cache \
    ca-certificates \
    fd \
    poppler-utils \
    ripgrep
COPY --from=builder /usr/local/cargo/bin/writestead /usr/local/bin/writestead
RUN npm install -g @llamaindex/liteparse obsidian-headless
EXPOSE 8765
VOLUME /vault
CMD ["writestead", "start", "--foreground", "--host", "0.0.0.0"]
