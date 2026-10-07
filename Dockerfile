# route66-snapbot: one amd64 Lambda image holding the Rust handler (GH #4115)
# with Chromium embedded in process through CEF (owner 2026-10-07), and the
# Tesseract English model. No Node, npm, Playwright or external Chromium.

# ---------------------------------------------------------------------------
# native-build: glibc toolchain, static Leptonica + Tesseract, CEF, the handler.
# WHY GLIBC (owner 2026-10-07: "The toolchain moves to glibc"): CEF ships as a
# glibc libcef.so that the handler links and loads in process; a static musl
# binary cannot load it. Amazon Linux 2023 is the Lambda runtime's own userland.
# ---------------------------------------------------------------------------
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:12052e9b5d3fd85769abbdd863dd038e1890c9ace31d5fdbe1afa78eda97d061 AS native-build

ARG LEPTONICA_VERSION=1.87.0
ARG TESSERACT_VERSION=5.5.2
# The AWS SDK's MSRV is newer than the distro rustc, so the toolchain is
# rustup's pinned build.
ARG RUST_VERSION=1.97.0

RUN dnf install -y --setopt=install_weak_deps=False gcc gcc-c++ make cmake ninja-build clang clang-devel llvm-devel \
        autoconf automake libtool pkgconf-pkg-config tar gzip bzip2 xz findutils perl which >/dev/null \
    && dnf clean all \
    && curl -fsSL -o /tmp/rustup-init https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init \
    && chmod +x /tmp/rustup-init \
    && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain ${RUST_VERSION} \
    && rm /tmp/rustup-init
ENV PATH=/root/.cargo/bin:${PATH}

WORKDIR /tmp/build
# Leptonica without any image codec: the engine hands Tesseract raw gray
# memory (SetImage), so no PNG/JPEG/GIF/TIFF reader is ever linked. Each native
# build deletes its source tree in the same RUN, so the layer holds only the
# installed prefix.
RUN curl -fsSL "https://github.com/DanBloomberg/leptonica/archive/refs/tags/${LEPTONICA_VERSION}.tar.gz" \
        | tar -xz \
    && cd "leptonica-${LEPTONICA_VERSION}" \
    && ./autogen.sh \
    && ./configure --prefix=/opt/snapbot-static --disable-shared --enable-static --with-pic \
        --without-zlib --without-libpng --without-jpeg --without-giflib \
        --without-libtiff --without-libwebp --without-libopenjpeg \
    && make -j"$(nproc)" \
    && make install \
    && cd / && rm -rf /tmp/build/leptonica-*

RUN curl -fsSL "https://github.com/tesseract-ocr/tesseract/archive/refs/tags/${TESSERACT_VERSION}.tar.gz" \
        | tar -xz \
    && cd "tesseract-${TESSERACT_VERSION}" \
    && ./autogen.sh \
    && PKG_CONFIG_PATH=/opt/snapbot-static/lib/pkgconfig ./configure \
        --prefix=/opt/snapbot-static --disable-shared --enable-static --with-pic \
        --disable-openmp --disable-graphics --disable-training-tools --without-archive --without-curl \
    && make -j"$(nproc)" \
    && make install \
    && cd / && rm -rf /tmp/build/tesseract-*

# Tesseract and Leptonica link statically; the C++ runtime and glibc stay
# dynamic (the runtime image has both). The rpath names the one directory the
# CEF runtime is installed in. CEF_PATH is where cef-dll-sys downloads the CEF
# build matching the pinned `cef` crate.
ENV PKG_CONFIG_PATH=/opt/snapbot-static/lib/pkgconfig \
    PKG_CONFIG_ALL_STATIC=1 \
    CEF_PATH=/opt/cef-dist \
    RUSTFLAGS="-C link-arg=-lstdc++ -C link-arg=-Wl,-rpath,/opt/snapbot/cef"

WORKDIR /src
# Dependencies first, against stub sources, so a source edit reuses them. The
# CEF distribution is installed as one flat runtime directory: libcef.so beside
# its .pak/.bin/.dat resources and locales/, where CEF looks for them on Linux.
COPY Cargo.toml Cargo.lock ./
COPY ocrd-rust/Cargo.toml ocrd-rust/Cargo.toml
COPY snapbot/Cargo.toml snapbot/Cargo.toml
RUN mkdir -p ocrd-rust/src snapbot/src \
    && echo '' > ocrd-rust/src/lib.rs \
    && echo 'fn main() {}' > snapbot/src/main.rs \
    && cargo build --release --locked \
    && rm -rf ocrd-rust/src snapbot/src \
    && lib="$(find /opt/cef-dist -name libcef.so -print -quit)" \
    && res="$(find /opt/cef-dist -name icudtl.dat -print -quit)" \
    && test -n "$lib" && test -n "$res" \
    && mkdir -p /opt/snapbot/cef \
    && cp -a "$(dirname "$lib")"/. /opt/snapbot/cef/ \
    && cp -an "$(dirname "$res")"/. /opt/snapbot/cef/
COPY ocrd-rust/src ocrd-rust/src
COPY snapbot/src snapbot/src
RUN touch ocrd-rust/src/lib.rs snapbot/src/main.rs \
    && cargo build --release --locked \
    && mkdir -p /out/tessdata \
    && cp target/release/snapbot /out/bootstrap \
    && /out/bootstrap --version \
    && curl -fsSL -o /out/tessdata/eng.traineddata \
        "https://github.com/tesseract-ocr/tessdata_fast/raw/4.1.0/eng.traineddata"

# ---------------------------------------------------------------------------
# fonts: the font set the baselines were rendered with. Only fonts.tar.br is
# taken from the integrity-checked @sparticuz/chromium 149.0.0 tarball; its
# Chromium binary is no longer used (CEF replaced it).
# ---------------------------------------------------------------------------
FROM public.ecr.aws/docker/library/golang:1.27-alpine@sha256:4c9fe60190a2a3350ddc51de80d0224b8a6698d12bdfc999fee45ea9d6c46dbc AS fonts

ARG SPARTICUZ_VERSION=149.0.0
ARG SPARTICUZ_SHA512=2NECBVKlUA9xIUXb4fT8OoGKdAJs+I2tNYscO8FwcxCKCWA7FmpPI0fdVxGJoIJglrFZYn+4YEJqChq4rdrxQg==

RUN apk add --no-cache brotli curl openssl tar \
    && curl -fsSL -o /tmp/chromium.tgz "https://registry.npmjs.org/@sparticuz/chromium/-/chromium-${SPARTICUZ_VERSION}.tgz" \
    && test "$(openssl dgst -sha512 -binary /tmp/chromium.tgz | base64 -w0)" = "${SPARTICUZ_SHA512}" \
    && mkdir -p /tmp/pkg /out/fonts \
    && tar -xzf /tmp/chromium.tgz -C /tmp/pkg \
    && brotli -dc /tmp/pkg/package/bin/fonts.tar.br | tar -x -C /out/fonts \
    && grep -rl '/tmp/fonts' /out/fonts | xargs -r sed -i 's#/tmp/fonts#/opt/snapbot/fonts#g'

# ---------------------------------------------------------------------------
# runtime: the Lambda custom-runtime base, plus the shared libraries libcef.so
# needs (Lambda's base userland ships none of the X11/NSS/ATK set). The binary
# is the bootstrap; the CMD (ImageConfig.Command) names the handler:
# index.handler, or fetch-hop.handler for the IPv6 fetch function. The local
# Kumo pool runs the same image as `/var/runtime/bootstrap kumo-runtime`.
# ---------------------------------------------------------------------------
FROM public.ecr.aws/lambda/provided:al2023 AS runtime

RUN (command -v dnf >/dev/null && PM=dnf || PM=microdnf; \
     $PM install -y nss nspr atk at-spi2-atk at-spi2-core cups-libs libdrm libxkbcommon libXcomposite libXdamage \
        libXrandr libXfixes libXext libX11 libxcb mesa-libgbm pango cairo alsa-lib dbus-libs expat glib2 \
        libxshmfence fontconfig freetype libstdc++ >/dev/null \
     && $PM clean all)

COPY --from=native-build /out/bootstrap /var/runtime/bootstrap
COPY --from=native-build /out/tessdata /opt/snapbot/tessdata
COPY --from=native-build /opt/snapbot/cef /opt/snapbot/cef
COPY --from=fonts /out/fonts /opt/snapbot/fonts
ENV TESSDATA_PREFIX=/opt/snapbot/tessdata \
    FONTCONFIG_PATH=/opt/snapbot/fonts

# The ldd assertion (owner 2026-10-07: the static-musl `! ldd` is replaced):
# every shared library the handler and libcef.so need resolves in this
# userland; a single "not found" fails the build. Then the binary runs, and
# the embedded Chromium paints and the engine reads the frame back.
RUN ! ldd /var/runtime/bootstrap /opt/snapbot/cef/libcef.so | grep 'not found' \
    && /var/runtime/bootstrap --version \
    && /var/runtime/bootstrap probe

CMD ["index.handler"]
