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

RUN make ARCH=arm64 defconfig \
    && scripts/kconfig/merge_config.sh -m .config kernel/configs/nebula.config \
    && make ARCH=arm64 olddefconfig

RUN make ARCH=arm64 -j"$(nproc)" Image

FROM scratch AS export
ARG KERNEL_VERSION=6.12.58
COPY --from=build /src/arch/arm64/boot/Image /Image
COPY --from=build /src/.config /kernel.config
