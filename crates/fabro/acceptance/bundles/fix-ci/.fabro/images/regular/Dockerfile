# Immutable sandbox-images release from lithoscomputer/sandbox-images@b253b0b6004f.
# Keep the added versions aligned with Interface's .node-version and
# packageManager fields.
FROM ghcr.io/lithoscomputer/ubuntu-24.04:slim-b253b0b6004f@sha256:c7ea8097107a751bc9e646ef8c4cb629dc95939edc4ce79a6315d32c4558c2df

ARG NODE_VERSION=22.22.0
ARG YARN_VERSION=4.12.0

RUN set -eu; \
    case "$(uname -m)" in \
      x86_64) node_arch=x64 ;; \
      aarch64 | arm64) node_arch=arm64 ;; \
      *) \
        echo "Unsupported architecture: $(uname -m)" >&2; \
        exit 1 \
        ;; \
    esac; \
    node_archive="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz"; \
    curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/${node_archive}" \
      -o "/tmp/${node_archive}"; \
    curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt" \
      -o /tmp/SHASUMS256.txt; \
    grep " ${node_archive}\$" /tmp/SHASUMS256.txt > /tmp/node-checksum.txt; \
    cd /tmp; \
    sha256sum --check --strict /tmp/node-checksum.txt; \
    tar -xJf "/tmp/${node_archive}" -C /usr/local --strip-components=1 \
      --no-same-owner; \
    rm -f "/tmp/${node_archive}" /tmp/SHASUMS256.txt /tmp/node-checksum.txt; \
    node --version

ENV COREPACK_HOME=/usr/local/corepack \
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0

RUN set -eu; \
    corepack enable; \
    corepack prepare "yarn@${YARN_VERSION}" --activate; \
    chmod -R a+rX "$COREPACK_HOME"; \
    yarn --version
