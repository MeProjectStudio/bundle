FROM gcr.io/distroless/static-debian12:nonroot

ARG TARGETARCH
COPY --chmod=755 bundle-${TARGETARCH} /usr/bin/bundle

ARG BUILD_DATE
ARG GIT_REVISION
ARG VERSION

LABEL org.opencontainers.image.created="${BUILD_DATE}"
LABEL org.opencontainers.image.revision="${GIT_REVISION}"
LABEL org.opencontainers.image.version="${VERSION}"
LABEL org.opencontainers.image.vendor="MeProject Team"
LABEL org.opencontainers.image.authors="MeProject Team, bundle contributors"
LABEL org.opencontainers.image.source="https://github.com/MeProjectStudio/bundle"
LABEL org.opencontainers.image.description="Declarative mod management system for Minecraft servers using OCI images"
LABEL org.opencontainers.image.licenses="GPL-3.0-only"

WORKDIR /

ENTRYPOINT ["bundle"]
