# fpython toolchain image: Alpine (musl) + C compiler + the fpython compiler.
# Programs built here with `--static` are fully static and run in FROM scratch images.
#
#   docker build -f docker/Dockerfile -t fpython:latest .
FROM rust:1-alpine AS toolchain-build
RUN apk add --no-cache musl-dev
WORKDIR /fpython
COPY Cargo.toml ./
COPY src ./src
COPY runtime ./runtime
COPY stdlib ./stdlib
RUN cargo build --release && strip target/release/fpython

FROM alpine:3.22
RUN apk add --no-cache gcc musl-dev
COPY --from=toolchain-build /fpython/target/release/fpython /usr/local/bin/fpython
ENV FPY_CACHE_DIR=/var/cache/fpython
# Warm the runtime cache so builds don't need a writable HOME.
RUN mkdir -p /var/cache/fpython && printf 'print("ok")\n' > /tmp/w.fpy && fpython build --static /tmp/w.fpy -o /tmp/w && /tmp/w && rm -f /tmp/w /tmp/w.fpy
WORKDIR /src
ENTRYPOINT ["fpython"]
CMD ["--help"]
