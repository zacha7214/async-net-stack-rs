# Build environment for scripts/build-kernel-lab.py.
#
# This image is never part of the guest: it holds the toolchain that produces the
# bundle, while the bundle's own userspace is the statically linked busybox and
# the xdp_vm_rx receiver the builder copies into the initramfs.
#
# Built on demand by scripts/lab_builders.py (--builder docker). The platform is
# passed at build time, so an ARM64 host builds and runs this natively.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
# busybox-static matters: the initramfs carries no shared libraries beyond the
# ones the receiver itself needs, so a dynamically linked busybox would not run.
RUN apt-get update && apt-get install -y --no-install-recommends \
        bc \
        binutils \
        bison \
        build-essential \
        busybox-static \
        ca-certificates \
        cpio \
        curl \
        file \
        flex \
        git \
        kmod \
        libelf-dev \
        libssl-dev \
        python3 \
    && rm -rf /var/lib/apt/lists/*

# The kernel tree arrives as a bind mount owned by the host user. git refuses to
# read a repository owned by somebody else, and the builder asks it for the
# source commit and diff that go into manifest.json.
RUN git config --global --add safe.directory '*'

# Rust builds the guest receiver. A login shell is used to run the build, so the
# PATH goes in profile.d rather than ENV alone, which /etc/profile would replace.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable \
    && chmod -R a+rX "$RUSTUP_HOME" "$CARGO_HOME" \
    && printf 'export PATH=/usr/local/cargo/bin:$PATH\n' > /etc/profile.d/cargo.sh

WORKDIR /work
