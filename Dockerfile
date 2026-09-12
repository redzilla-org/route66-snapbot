# route66-snapbot ships Chromium, attestation, and OCR as one amd64 Lambda
# image. Building Tesseract and Leptonica statically keeps their native-library
# closure out of the Lambda base and makes the final runtime self-contained.
FROM public.ecr.aws/docker/library/golang:1.27-alpine@sha256:4c9fe60190a2a3350ddc51de80d0224b8a6698d12bdfc999fee45ea9d6c46dbc AS ocr-build

ARG LEPTONICA_VERSION=1.87.0
ARG TESSERACT_VERSION=5.5.2

# These packages exist only in the build stage. The final Lambda image receives
# one static executable and the English model, never a compiler or package cache.
RUN apk add --no-cache rust cargo build-base linux-headers musl-dev pkgconf \
        clang clang-dev llvm-dev ca-certificates curl autoconf automake libtool \
        zlib-dev zlib-static libpng-dev libpng-static libjpeg-turbo-dev \
        libjpeg-turbo-static giflib-dev giflib-static libstdc++-dev

WORKDIR /tmp/build
RUN curl -fsSL "https://github.com/DanBloomberg/leptonica/archive/refs/tags/${LEPTONICA_VERSION}.tar.gz" \
        | tar -xz \
    && cd "leptonica-${LEPTONICA_VERSION}" \
    && ./autogen.sh \
    && ./configure --prefix=/opt/snapbot-static --disable-shared --enable-static \
        --without-libtiff --without-libwebp --without-libopenjpeg \
    && make -j"$(nproc)" \
    && make install

RUN curl -fsSL "https://github.com/tesseract-ocr/tesseract/archive/refs/tags/${TESSERACT_VERSION}.tar.gz" \
        | tar -xz \
    && cd "tesseract-${TESSERACT_VERSION}" \
    && ./autogen.sh \
    && PKG_CONFIG_PATH=/opt/snapbot-static/lib/pkgconfig ./configure \
        --prefix=/opt/snapbot-static --disable-shared --enable-static \
        --disable-openmp --disable-graphics --disable-training-tools \
    && make -j"$(nproc)" \
    && make install

COPY ocrd-rust /src
WORKDIR /src

# The sys crates insert a late dynamic-link switch. This wrapper restores static
# mode for the C++ runtime so `ldd` below becomes a hard image-build assertion.
RUN printf '%s\n' \
        '#!/bin/sh' \
        'exec g++ -static -static-libstdc++ -static-libgcc "$@" -Wl,-Bstatic' \
        > /usr/local/bin/snapbot-static-cxx-link \
    && chmod +x /usr/local/bin/snapbot-static-cxx-link \
    && PKG_CONFIG_PATH=/opt/snapbot-static/lib/pkgconfig PKG_CONFIG_ALL_STATIC=1 \
        cargo rustc --release --locked --target-dir target-static --bin snapbot-ocr-worker -- \
        -C linker=/usr/local/bin/snapbot-static-cxx-link \
        -C target-feature=+crt-static -C relocation-model=static -C link-arg=-no-pie \
    && mkdir -p /out/tessdata \
    && cp target-static/release/snapbot-ocr-worker /out/snapbot-ocr-worker \
    && curl -fsSL -o /out/tessdata/eng.traineddata \
        "https://github.com/tesseract-ocr/tessdata_fast/raw/4.1.0/eng.traineddata" \
    && ! ldd /out/snapbot-ocr-worker \
    && /out/snapbot-ocr-worker --version

FROM public.ecr.aws/lambda/nodejs:22 AS runtime

# npm's production closure contains the pinned Sparticuz Chromium build and
# puppeteer driver. AWS SDK v3 remains supplied by the managed Lambda base, as
# it was for the imported zip deployment.
COPY attestor/attestor/package.json attestor/attestor/package-lock.json ${LAMBDA_TASK_ROOT}/
RUN cd ${LAMBDA_TASK_ROOT} \
    && npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
    && npm cache clean --force

COPY attestor/attestor/index.js attestor/attestor/kumo-runtime.js attestor/public-key.json ${LAMBDA_TASK_ROOT}/
COPY --from=ocr-build /out/snapbot-ocr-worker /opt/snapbot/snapbot-ocr-worker
COPY --from=ocr-build /out/tessdata /opt/snapbot/tessdata

# Both checks execute in the final userland. A wrong architecture, dynamic
# native dependency, malformed handler, or missing executable fails the build.
ENV TESSDATA_PREFIX=/opt/snapbot/tessdata
ENV SNAPBOT_OCR_WORKER=/opt/snapbot/snapbot-ocr-worker
RUN /opt/snapbot/snapbot-ocr-worker --version \
    && node --check ${LAMBDA_TASK_ROOT}/index.js \
    && node --check ${LAMBDA_TASK_ROOT}/kumo-runtime.js

CMD ["index.handler"]
