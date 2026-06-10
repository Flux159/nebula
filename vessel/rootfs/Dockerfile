# Builds the Vessel root filesystem image (ext4) from an Alpine base plus the
# Nebula guest binaries. No loop devices: mkfs.ext4 -d populates from a dir.
#
# FLAVOR selects what ships inside (embedders pick their size/feature point):
#   full    — docker engine + k3s (the default engine image). No in-guest
#             CLIs: docker/kubectl/helm live on the HOST (bundled in the
#             .app) and talk over the socket proxy / API forward.
#   docker  — containerd + dockerd only (no k3s)
#   minimal — agent only (clean Linux microVM; nebula vessels/sandbox use)
FROM alpine:3.22 AS rootfs
ARG FLAVOR=full

RUN apk add --no-cache e2fsprogs ca-certificates \
    && mkdir -p /var/lib/nebula /run /tmp /etc/docker /etc/containerd /media/rosetta

RUN if [ "$FLAVOR" != "minimal" ]; then \
        apk add --no-cache docker-engine containerd containerd-ctr runc iptables ip6tables; \
    fi

# k3s: full flavor only (it bundles its own kubectl for in-guest use).
ARG K3S_VERSION=v1.35.5%2Bk3s1
RUN if [ "$FLAVOR" = "full" ]; then \
        wget -qO /usr/local/bin/k3s \
            "https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/k3s-arm64" \
        && chmod 755 /usr/local/bin/k3s; \
    fi

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
