# Builds the Vessel root filesystem image (ext4) from an Alpine base plus the
# Nebula guest binaries. No loop devices: mkfs.ext4 -d populates from a dir.
#
# FLAVOR selects what ships inside (embedders pick their size/feature point):
#   full    — docker engine + k3s (the default engine image). No in-guest
#             CLIs: docker/kubectl/helm live on the HOST (bundled in the
#             .app) and talk over the socket proxy / API forward.
#   docker  — containerd + dockerd only (no k3s)
#   slim    — nebula-slim engine (slimd) only: no dockerd/containerd/k3s, no Go
#             binaries. Just iproute2+iptables for slimd's networking. The
#             docker/kubectl/helm clients are the standalone *-slim host
#             binaries (built from the slim workspace). Smallest functional
#             container+k8s+helm image.
#   minimal — agent only (clean Linux microVM; nebula vessels/sandbox use)
FROM alpine:3.22 AS rootfs
ARG FLAVOR=full

RUN apk add --no-cache e2fsprogs ca-certificates \
    && mkdir -p /var/lib/nebula /run /tmp /etc/docker /etc/containerd /media/rosetta

# slim needs only the networking userland slimd shells out to.
RUN if [ "$FLAVOR" = "slim" ]; then \
        apk add --no-cache iproute2 iptables ip6tables ca-certificates; \
    elif [ "$FLAVOR" != "minimal" ]; then \
        apk add --no-cache docker-engine containerd containerd-ctr runc iptables ip6tables; \
    fi

# slimd: the nebula-slim engine binary, staged into vessel/rootfs/bin by
# build-rootfs.sh when FLAVOR=slim (built from ~/Projects/nebula-slim). `pause`
# is the tiny static pod-sandbox binary slimd registers as nebula/pause:slim.
# The .slimkeep anchor (always present) lets the slimd*/pause* wildcards match
# zero files for non-slim flavors without erroring (canonical optional-COPY trick).
COPY bin/.slimkeep bin/slimd* bin/pause* /usr/local/bin/
RUN if [ -f /usr/local/bin/slimd ]; then chmod 755 /usr/local/bin/slimd; fi
RUN if [ -f /usr/local/bin/pause ]; then chmod 755 /usr/local/bin/pause; fi

# k3s: full flavor only (it bundles its own kubectl for in-guest use).
# Release asset name: k3s-arm64 for arm64; plain `k3s` for amd64.
ARG K3S_VERSION=v1.35.5%2Bk3s1
ARG K3S_ARCH=arm64
RUN if [ "$FLAVOR" = "full" ]; then \
        if [ "$K3S_ARCH" = "amd64" ]; then K3S_ASSET=k3s; else K3S_ASSET="k3s-${K3S_ARCH}"; fi \
        && wget -qO /usr/local/bin/k3s \
            "https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/${K3S_ASSET}" \
        && chmod 755 /usr/local/bin/k3s; \
    fi

# Embedder customization (staged by build-rootfs.sh; empty by default):
# overlay/ is copied over / (embedder files win), then setup.sh runs inside
# the image. Both happen BEFORE the Nebula guest binaries land below, so the
# boot/manage contract can't be broken by an overlay.
COPY overlay/ /
COPY setup.sh /nebula-setup.sh
RUN sh /nebula-setup.sh && rm -f /nebula-setup.sh

# Nebula guest binaries last — always authoritative.
COPY bin/vessel-init /sbin/nebula-init
COPY bin/vessel-agent /usr/bin/vessel-agent
# Display bridge: serves the guest framebuffer on vsock:1027 and replays host
# input through /dev/uinput. nebula-init starts it when this file is present.
COPY bin/vessel-display /usr/bin/vessel-display
RUN chmod 755 /sbin/nebula-init /usr/bin/vessel-agent /usr/bin/vessel-display

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
