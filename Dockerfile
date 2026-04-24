FROM node:22-alpine
RUN npm install -g @ahkohd/writestead
EXPOSE 8765
CMD ["writestead", "start", "--foreground"]
