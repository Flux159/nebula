# Builds the Vessel root filesystem image (ext4) from an Alpine base plus the
# Nebula guest binaries. No loop devices: mkfs.ext4 -d populates from a dir.
FROM alpine:3.22 AS rootfs

RUN apk add --no-cache e2fsprogs ca-certificates \
    && mkdir -p /var/lib/nebula /run /tmp

# Nebula guest binaries (cross-built on the host, passed via build context).
COPY bin/vessel-init /sbin/nebula-init
COPY bin/vessel-agent /usr/bin/vessel-agent
RUN chmod 755 /sbin/nebula-init /usr/bin/vessel-agent

FROM alpine:3.22 AS build
RUN apk add --no-cache e2fsprogs
COPY --from=rootfs / /rootfs
ARG ROOTFS_SIZE_MB=1024
RUN truncate -s "${ROOTFS_SIZE_MB}M" /rootfs.img \
    && mkfs.ext4 -q -L nebula-root -d /rootfs /rootfs.img

FROM scratch AS export
COPY --from=build /rootfs.img /rootfs.img
