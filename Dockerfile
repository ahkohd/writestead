FROM node:22-alpine

RUN apk add --no-cache \
    ca-certificates \
    fd \
    poppler-utils \
    ripgrep

RUN npm install -g @ahkohd/writestead @llamaindex/liteparse obsidian-headless

EXPOSE 8765
VOLUME /vault
CMD ["writestead", "start", "--foreground", "--host", "0.0.0.0"]
