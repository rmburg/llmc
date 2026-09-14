FROM docker.io/library/archlinux

ARG PACKAGES="opencode git rustup base-devel less clang"

RUN pacman -Sy --noconfirm ${PACKAGES} && yes | pacman -Scc

ARG USERNAME=user
ARG UID=1000
ARG GID=1000

RUN groupadd -g ${GID} ${USERNAME} && \
    useradd \
        -m \
        -u ${UID} \
        -g ${GID} \
        -s /bin/bash \
        ${USERNAME}

RUN printf '%s ALL=(root) NOPASSWD: /usr/bin/pacman\n' "$USERNAME" > /etc/sudoers.d/allow-pacman && \
    chmod 0440 /etc/sudoers.d/allow-pacman && \
    visudo -cf /etc/sudoers.d/allow-pacman

USER ${USERNAME}

RUN if command -v rustup >/dev/null 2>&1; then \
      rustup default stable && \
      rustup component add rust-analyzer; \
    fi

ENV PATH=/usr/lib/rustup/bin:$PATH
