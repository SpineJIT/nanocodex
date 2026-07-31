# syntax=docker/dockerfile:1

FROM node:24-bookworm AS node

FROM rust:1.97-bookworm AS wasm-builder
COPY --from=node /usr/local/ /usr/local/
WORKDIR /src
COPY Cargo.lock ./
RUN rustup target add wasm32-unknown-unknown \
    && wasm_bindgen_version="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/"/, "", $3); print $3; exit }' Cargo.lock)" \
    && test -n "$wasm_bindgen_version" \
    && cargo install wasm-bindgen-cli --version "$wasm_bindgen_version" --locked
COPY . .
RUN ./scripts/build-js-package.sh

FROM node:24-bookworm-slim AS app-builder
WORKDIR /app
COPY --from=wasm-builder /src/js/bindings ./js/bindings
COPY examples/rivet-actors/package.json examples/rivet-actors/package-lock.json ./examples/rivet-actors/
RUN npm ci --prefix examples/rivet-actors
COPY examples/rivet-actors ./examples/rivet-actors
RUN npm run build --prefix examples/rivet-actors \
    && npm prune --prefix examples/rivet-actors --omit=dev

FROM node:24-bookworm-slim
ENV NODE_ENV=production
WORKDIR /app
COPY --from=app-builder --chown=node:node /app/js/bindings ./js/bindings
COPY --from=app-builder --chown=node:node /app/examples/rivet-actors/package.json ./examples/rivet-actors/package.json
COPY --from=app-builder --chown=node:node /app/examples/rivet-actors/package-lock.json ./examples/rivet-actors/package-lock.json
COPY --from=app-builder --chown=node:node /app/examples/rivet-actors/node_modules ./examples/rivet-actors/node_modules
COPY --from=app-builder --chown=node:node /app/examples/rivet-actors/dist ./examples/rivet-actors/dist
COPY --from=app-builder --chown=node:node /app/examples/rivet-actors/web ./examples/rivet-actors/web
USER node
WORKDIR /app/examples/rivet-actors
CMD ["node", "dist/server.js"]
