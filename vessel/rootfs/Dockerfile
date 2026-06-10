# Builds the Vessel root filesystem image (ext4) from an Alpine base plus the
# Nebula guest binaries. No loop devices: mkfs.ext4 -d populates from a dir.
FROM alpine:3.22 AS rootfs

RUN apk add --no-cache \
    e2fsprogs ca-certificates \
    docker-engine docker-cli containerd containerd-ctr runc nerdctl \
    iptables ip6tables \
    && mkdir -p /var/lib/nebula /run /tmp /etc/docker /etc/containerd /media/rosetta

# Nebula guest binaries (cross-built on the host, passed via build context).
COPY bin/vessel-init /sbin/nebula-init
COPY bin/vessel-agent /usr/bin/vessel-agent
RUN chmod 755 /sbin/nebula-init /usr/bin/vessel-agent

# containerd: persistent state on the data disk, shared by dockerd (and k3s later).
RUN printf 'version = 2\nroot = "/var/lib/nebula/containerd"\nstate = "/run/containerd"\ndisabled_plugins = ["io.containerd.grpc.v1.cri"]\n' \
    > /etc/containerd/config.toml

# dockerd: containerd image store so docker/nerdctl/k3s see one image set.
RUN printf '{\n  "data-root": "/var/lib/nebula/docker",\n  "userland-proxy-path": "/usr/bin/docker-proxy",\n  "dns": ["172.17.0.1"],\n  "features": { "containerd-snapshotter": true }\n}\n' \
    > /etc/docker/daemon.json

FROM alpine:3.22 AS build
RUN apk add --no-cache e2fsprogs
COPY --from=rootfs / /rootfs
ARG ROOTFS_SIZE_MB=2048
RUN truncate -s "${ROOTFS_SIZE_MB}M" /rootfs.img \
    && mkfs.ext4 -q -L nebula-root -d /rootfs /rootfs.img

FROM scratch AS export
COPY --from=build /rootfs.img /rootfs.img
