# Multi-stage build: compile with the fpython toolchain, ship a FROM scratch image.
#   docker build -f docker/Dockerfile -t fpython:latest .            # once, from the repo root
#   docker build -t fpython-hello examples/hello-docker
#   docker run --rm fpython-hello 1000
ARG FPYTHON_IMAGE=fpython:latest
FROM ${FPYTHON_IMAGE} AS build
COPY app.fpy .
RUN fpython build --release --static app.fpy -o /out/app

FROM scratch
COPY --from=build /out/app /app
ENTRYPOINT ["/app"]
