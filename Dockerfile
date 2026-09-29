# route66-snapbot: one amd64 Lambda image holding one static Rust binary (the
# handler, GH #4115), Chromium, and the Tesseract English model. No Node, npm
# or Playwright: the owner burned the Node coordinator on 2026-09-29.

# ---------------------------------------------------------------------------
# native-build: static Leptonica + Tesseract, and the static musl handler.
# Building Tesseract statically keeps its native closure out of the runtime
# image; the handler links it in-process (no OCR child, no pipe).
# ---------------------------------------------------------------------------
FROM public.ecr.aws/docker/library/golang:1.27-alpine@sha256:4c9fe60190a2a3350ddc51de80d0224b8a6698d12bdfc999fee45ea9d6c46dbc AS native-build

ARG LEPTONICA_VERSION=1.87.0
ARG TESSERACT_VERSION=5.5.2
# The AWS SDK's MSRV (1.94.1) is newer than Alpine's packaged rustc, so the
# toolchain is rustup's pinned musl build.
ARG RUST_VERSION=1.97.0

RUN apk add --no-cache build-base linux-headers musl-dev pkgconf \
        clang clang-dev llvm-dev ca-certificates curl autoconf automake libtool \
        zlib-dev zlib-static libpng-dev libpng-static libjpeg-turbo-dev \
        libjpeg-turbo-static giflib-dev giflib-static libstdc++-dev cmake perl \
    && curl -fsSL -o /tmp/rustup-init https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-musl/rustup-init \
    && chmod +x /tmp/rustup-init \
    && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain ${RUST_VERSION} \
    && rm /tmp/rustup-init
ENV PATH=/root/.cargo/bin:${PATH}

WORKDIR /tmp/build
# Each native build deletes its source tree in the same RUN, so the layer
# holds only the installed prefix.
RUN curl -fsSL "https://github.com/DanBloomberg/leptonica/archive/refs/tags/${LEPTONICA_VERSION}.tar.gz" \
        | tar -xz \
    && cd "leptonica-${LEPTONICA_VERSION}" \
    && ./autogen.sh \
    && ./configure --prefix=/opt/snapbot-static --disable-shared --enable-static \
        --without-libtiff --without-libwebp --without-libopenjpeg \
    && make -j"$(nproc)" \
    && make install \
    && cd / && rm -rf /tmp/build/leptonica-*

RUN curl -fsSL "https://github.com/tesseract-ocr/tesseract/archive/refs/tags/${TESSERACT_VERSION}.tar.gz" \
        | tar -xz \
    && cd "tesseract-${TESSERACT_VERSION}" \
    && ./autogen.sh \
    && PKG_CONFIG_PATH=/opt/snapbot-static/lib/pkgconfig ./configure \
        --prefix=/opt/snapbot-static --disable-shared --enable-static \
        --disable-openmp --disable-graphics --disable-training-tools \
    && make -j"$(nproc)" \
    && make install \
    && cd / && rm -rf /tmp/build/tesseract-*

# The sys crates insert a late dynamic-link switch; this wrapper restores
# static mode for the C++ runtime so `ldd` below is a hard build assertion.
# The flags ride on the explicit --target so build scripts and proc macros
# (host artifacts) link normally.
RUN printf '%s\n' \
        '#!/bin/sh' \
        'exec g++ -static -static-libstdc++ -static-libgcc "$@" -Wl,-Bstatic' \
        > /usr/local/bin/snapbot-static-cxx-link \
    && chmod +x /usr/local/bin/snapbot-static-cxx-link
ENV PKG_CONFIG_PATH=/opt/snapbot-static/lib/pkgconfig \
    PKG_CONFIG_ALL_STATIC=1 \
    CARGO_BUILD_TARGET=x86_64-unknown-linux-musl \
    RUSTFLAGS="-C linker=/usr/local/bin/snapbot-static-cxx-link -C target-feature=+crt-static -C relocation-model=static -C link-arg=-no-pie"

WORKDIR /src
# Dependencies first, against stub sources, so a source edit reuses them.
COPY Cargo.toml Cargo.lock ./
COPY ocrd-rust/Cargo.toml ocrd-rust/Cargo.toml
COPY snapbot/Cargo.toml snapbot/Cargo.toml
RUN mkdir -p ocrd-rust/src snapbot/src \
    && echo '' > ocrd-rust/src/lib.rs \
    && echo 'fn main() {}' > snapbot/src/main.rs \
    && cargo build --release --locked \
    && rm -rf ocrd-rust/src snapbot/src
COPY ocrd-rust/src ocrd-rust/src
COPY snapbot/src snapbot/src
RUN touch ocrd-rust/src/lib.rs snapbot/src/main.rs \
    && cargo build --release --locked \
    && mkdir -p /out/tessdata \
    && cp target/x86_64-unknown-linux-musl/release/snapbot /out/bootstrap \
    && ! ldd /out/bootstrap \
    && /out/bootstrap --version \
    && curl -fsSL -o /out/tessdata/eng.traineddata \
        "https://github.com/tesseract-ocr/tessdata_fast/raw/4.1.0/eng.traineddata"

# ---------------------------------------------------------------------------
# chromium: the @sparticuz/chromium 149.0.0 payload the Node handler ran,
# fetched as the published npm tarball (integrity-checked against the former
# lockfile's sha512) and inflated once at build time instead of into /tmp on
# every cold start. Same binary, same fonts, same flags: same pixels.
# ---------------------------------------------------------------------------
FROM public.ecr.aws/docker/library/golang:1.27-alpine@sha256:4c9fe60190a2a3350ddc51de80d0224b8a6698d12bdfc999fee45ea9d6c46dbc AS chromium

ARG SPARTICUZ_VERSION=149.0.0
ARG SPARTICUZ_SHA512=2NECBVKlUA9xIUXb4fT8OoGKdAJs+I2tNYscO8FwcxCKCWA7FmpPI0fdVxGJoIJglrFZYn+4YEJqChq4rdrxQg==

RUN apk add --no-cache brotli curl openssl tar \
    && curl -fsSL -o /tmp/chromium.tgz "https://registry.npmjs.org/@sparticuz/chromium/-/chromium-${SPARTICUZ_VERSION}.tgz" \
    && test "$(openssl dgst -sha512 -binary /tmp/chromium.tgz | base64 -w0)" = "${SPARTICUZ_SHA512}" \
    && mkdir -p /tmp/pkg /out/chromium/fonts /out/chromium/al2023 \
    && tar -xzf /tmp/chromium.tgz -C /tmp/pkg \
    && brotli -dc /tmp/pkg/package/bin/chromium.br > /out/chromium/chromium \
    && chmod 0755 /out/chromium/chromium \
    && brotli -dc /tmp/pkg/package/bin/swiftshader.tar.br | tar -x -C /out/chromium \
    && brotli -dc /tmp/pkg/package/bin/fonts.tar.br | tar -x -C /out/chromium/fonts \
    && brotli -dc /tmp/pkg/package/bin/al2023.tar.br | tar -x -C /out/chromium/al2023 \
    && grep -rl '/tmp/fonts' /out/chromium/fonts | xargs -r sed -i 's#/tmp/fonts#/opt/chromium/fonts#g' \
    && ls -R /out/chromium | head -n 60

# ---------------------------------------------------------------------------
# runtime: the Lambda custom-runtime base. The binary is the bootstrap; the
# CMD (ImageConfig.Command) names the handler: index.handler, or
# fetch-hop.handler for the IPv6 fetch function. The local Kumo pool runs the
# same image as `/var/runtime/bootstrap kumo-runtime`.
# ---------------------------------------------------------------------------
FROM public.ecr.aws/lambda/provided:al2023 AS runtime

COPY --from=native-build /out/bootstrap /var/runtime/bootstrap
COPY --from=native-build /out/tessdata /opt/snapbot/tessdata
COPY --from=chromium /out/chromium /opt/chromium
ENV TESSDATA_PREFIX=/opt/snapbot/tessdata

# Both checks execute in the final userland: the binary runs, and Chromium
# launches and navigates with the baked libraries and fonts.
RUN /var/runtime/bootstrap --version \
    && /var/runtime/bootstrap probe

CMD ["index.handler"]
