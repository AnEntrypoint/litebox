# Base image for litebox_packager --oci-image: Alpine plus a ready-to-use
# nodejs + python toolchain, so the packaged rootfs needs no further setup to
# run Node/Python agent workloads or install additional packages via apk.
FROM alpine:latest
RUN apk add --no-cache \
    nodejs \
    npm \
    python3 \
    py3-pip \
    build-base \
    ca-certificates
