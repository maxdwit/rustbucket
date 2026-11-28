# Builds ../static/editor.js (CodeMirror 6 + Yjs) with esbuild, from the committed lockfile only.
# Used by `make editor-bundle`; Node never runs on a server and is not part of the server image.
#   docker build -f editor/Dockerfile --target out --output type=local,dest=static editor
FROM node:22-alpine@sha256:1314fb30ec37592d5845e5a160d522c0e680972aa534549940b8d73bde88481a AS build
# the build runs unprivileged: the node image's own user
USER node
WORKDIR /tmp/e
COPY --chown=node:node package.json package-lock.json ./
# no install scripts of any package ever run
RUN npm ci --ignore-scripts --no-audit --no-fund
COPY --chown=node:node src ./src
RUN node_modules/.bin/esbuild src/main.js --bundle --minify --format=iife --target=es2020 --legal-comments=none --outfile=/tmp/editor.js

FROM scratch AS out
COPY --from=build /tmp/editor.js /editor.js
