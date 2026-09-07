# The artifact must run on this baseline without a compiler or source checkout.
FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash ca-certificates coreutils python3 \
    libdbus-1-3 libstdc++6 libgcc-s1 \
    pipewire-bin wl-clipboard libwayland-client0 libxkbcommon0 \
    libegl1 libgl1 libgl1-mesa-dri libfontconfig1 fonts-dejavu-core \
    dbus gnome-keyring libsecret-tools passwd \
    && rm -rf /var/lib/apt/lists/*
ARG TEST_UID=1000
ARG TEST_GID=1000
# Ubuntu 24.04 already has uid/gid 1000 (`ubuntu`). Reuse that identity.
RUN getent group "$TEST_GID" >/dev/null || groupadd --gid "$TEST_GID" cantrip \
    && getent passwd "$TEST_UID" >/dev/null || useradd --uid "$TEST_UID" --gid "$TEST_GID" --create-home cantrip
