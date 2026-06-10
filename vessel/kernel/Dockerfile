# Builds the Nebula guest kernel natively in an arm64 Linux container.
FROM debian:bookworm-slim AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    bc bison flex make gcc libc6-dev libssl-dev libelf-dev \
    python3 perl rsync kmod xz-utils curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG KERNEL_VERSION=6.12.58
WORKDIR /src
RUN curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" \
    | tar -xJ --strip-components=1

COPY nebula.fragment /src/kernel/configs/nebula.config
COPY apply-fragment.py /src/apply-fragment.py

# kconfig keeps the FIRST assignment per symbol, so the fragment must replace
# base assignments, not just append after them.
RUN make ARCH=arm64 defconfig \
    && python3 apply-fragment.py .config kernel/configs/nebula.config \
    && make ARCH=arm64 olddefconfig \
    && for opt in BRIDGE VETH VIRTIO_MMIO VIRTIO_FS VSOCKETS OVERLAY_FS BINFMT_MISC; do \
         grep -q "^CONFIG_${opt}=y" .config || { echo "FATAL: CONFIG_${opt} not =y"; exit 1; }; \
       done

RUN make ARCH=arm64 -j"$(nproc)" Image

FROM scratch AS export
ARG KERNEL_VERSION=6.12.58
COPY --from=build /src/arch/arm64/boot/Image /Image
COPY --from=build /src/.config /kernel.config
