FROM node:22-alpine

RUN apk add --no-cache \
    ca-certificates \
    fd \
    poppler-utils \
    ripgrep

RUN npm install -g @ahkohd/writestead @llamaindex/liteparse obsidian-headless

EXPOSE 8765
CMD ["writestead", "start", "--foreground"]
