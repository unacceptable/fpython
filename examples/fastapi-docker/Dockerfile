# FastAPI app in fpython -> FROM scratch image (native code + bundled CPython 3.14 fallback).
#   docker build -f docker/Dockerfile.python -t fpython-py:latest .      # once, from repo root
#   docker build -t fpython-fastapi examples/fastapi-docker
#   docker run --rm -p 8000:8000 fpython-fastapi
ARG FPYTHON_IMAGE=fpython-py:latest
FROM ${FPYTHON_IMAGE} AS build
RUN pip install --no-cache-dir fastapi uvicorn
COPY app.fpy .
RUN fpython build --release app.fpy -o /out/app && fpython bundle /out/app /rootfs

FROM scratch
COPY --from=build /rootfs /
EXPOSE 8000
ENTRYPOINT ["/app"]
