# The LiveStage appliance builder: compiles livestage-server for Alpine (musl)
# and assembles the bootable image. Nothing here needs root on the host or a
# privileged container: filesystems are written from directories (mke2fs -d,
# mtools), never mounted.
#
# Built and run by build.ps1 / build.sh next to this file.
ARG ALPINE_VERSION=3.24
FROM alpine:${ALPINE_VERSION}

RUN apk add --no-cache \
        build-base pkgconf linux-headers rust cargo alsa-lib-dev \
        mkinitfs efi-mkuki systemd-efistub binutils \
        dosfstools mtools e2fsprogs e2fsprogs-extra exfatprogs sfdisk \
        coreutils findutils tar xz zstd kmod

WORKDIR /work
