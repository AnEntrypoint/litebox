# Base image for litebox_packager --oci-image: Alpine plus a ready-to-use
# nodejs + python toolchain, so the packaged rootfs needs no further setup to
# run Node/Python agent workloads or install additional packages via apk.
# Pinned (not :latest): the fork_verify musl mallocng exclusion logic is derived
# from this specific musl version's on-disk struct layout (see FINDINGS.txt PASS
# 44/60/66) -- an unpinned :latest could silently drift the layout out from under
# that logic. 3.24.1 matches the currently-bundled alpine-rootfs.tar (musl
# 1.2.6-r2, confirmed via /etc/alpine-release and /lib/apk/db/installed).
FROM alpine:3.24.1
RUN apk add --no-cache \
    nodejs \
    npm \
    python3 \
    python3-dev \
    py3-pip \
    build-base \
    ca-certificates \
    git \
    bash \
    curl \
    openssh-client \
    tzdata

# litebox_runner_linux_userland runs the guest as UID/GID 1000 by default (a
# security-conscious default -- unlike litebox_runner_linux_on_windows_userland,
# which uses UID 0), so a real /etc/passwd entry for it is needed or any tool
# that does an NSS lookup on the current UID (e.g. `ssh -V`: "No user exists
# for uid 1000") fails even though the UID itself is correctly reported.
RUN adduser -D -u 1000 -h /home/litebox litebox
