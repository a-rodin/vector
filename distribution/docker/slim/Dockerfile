FROM debian:9-slim

RUN apt-get update -y
RUN apt-get install -y wget

ENV VECTOR_VERSION latest
RUN ARCH= && dpkgArch="$(dpkg --print-architecture)" && \
    case "${dpkgArch##*-}" in \
      amd64) ARCH='amd64';; \
      *)     echo "Unsupported architecture: ${dpkgArch}"; exit 1;; \
    esac && \
    wget --no-verbose https://packages.timber.io/vector/${VECTOR_VERSION}/vector-${VECTOR_VERSION}-${ARCH}.deb && \
    dpkg -i vector-${VECTOR_VERSION}-${ARCH}.deb && \
    rm -f vector-${VECTOR_VERSION}-${ARCH}.deb