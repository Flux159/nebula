# Builds the Nebula guest kernel in a Linux container.
#   ARCH=arm64  -> arch/arm64/boot/Image (raw, for VZ + libkrun/HVF)
#   ARCH=x86_64 -> vmlinux (ELF, for libkrun/KVM and later WHP)
# Cross-compiles automatically when the build container's arch differs.
FROM debian:bookworm-slim AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    bc bison flex make gcc libc6-dev libssl-dev libelf-dev \
    python3 perl rsync kmod xz-utils curl ca-certificates \
    gcc-x86-64-linux-gnu gcc-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

ARG KERNEL_VERSION=6.12.58
ARG ARCH=arm64
WORKDIR /src
RUN curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" \
    | tar -xJ --strip-components=1

COPY nebula.fragment /src/kernel/configs/nebula.config
COPY apply-fragment.py /src/apply-fragment.py

# kconfig keeps the FIRST assignment per symbol, so the fragment must replace
# base assignments, not just append after them.
RUN set -e; \
    case "$ARCH/$(uname -m)" in \
      arm64/aarch64|x86_64/x86_64) CROSS="" ;; \
      arm64/*)  CROSS=aarch64-linux-gnu- ;; \
      x86_64/*) CROSS=x86_64-linux-gnu- ;; \
    esac; \
    make ARCH="$ARCH" CROSS_COMPILE="$CROSS" defconfig \
    && python3 apply-fragment.py .config kernel/configs/nebula.config \
    && make ARCH="$ARCH" CROSS_COMPILE="$CROSS" olddefconfig \
    && for opt in BRIDGE VETH VIRTIO_MMIO VIRTIO_FS VSOCKETS OVERLAY_FS BINFMT_MISC; do \
         grep -q "^CONFIG_${opt}=y" .config || { echo "FATAL: CONFIG_${opt} not =y"; exit 1; }; \
       done

RUN set -e; \
    case "$ARCH/$(uname -m)" in \
      arm64/aarch64|x86_64/x86_64) CROSS="" ;; \
      arm64/*)  CROSS=aarch64-linux-gnu- ;; \
      x86_64/*) CROSS=x86_64-linux-gnu- ;; \
    esac; \
    mkdir -p /out; \
    if [ "$ARCH" = "arm64" ]; then \
      make ARCH="$ARCH" CROSS_COMPILE="$CROSS" -j"$(nproc)" Image \
      && cp arch/arm64/boot/Image /out/Image; \
    else \
      make ARCH="$ARCH" CROSS_COMPILE="$CROSS" -j"$(nproc)" vmlinux \
      && cp vmlinux /out/vmlinux; \
    fi; \
    cp .config /out/kernel.config

FROM scratch AS export
COPY --from=build /out/ /
