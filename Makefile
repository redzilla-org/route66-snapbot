# ocr-daemon build + publish.
#
# THE ARTIFACT NAME IS LANGUAGE-FREE, DELIBERATELY (owner 2026-08-24: "don't put
# the language name in the binary, keep it polymorphic"). What consumers install
# is `ocrd-<os>-<arch>`, and nothing in that name -- or in the protocol, or in
# the CLI -- reveals which implementation they got. That is the whole point of
# having two: either can be swapped in without a consumer noticing, so encoding
# the language in the filename would leak an implementation detail into every
# download URL, Dockerfile and install script, and make the swap a breaking
# change instead of a drop-in.
#
# The IMPLEMENTATION is named by the DIRECTORY instead: bin/rust/ and bin/go/
# hold identically-named binaries. That keeps both on disk at once without a
# name collision, and keeps the distinction where it belongs -- in the build
# tree, not in the shipped artifact.
VERSION ?= v0.1.0
REPO    ?= redzilla-org/ocr-daemon
BIN     := bin
ENG_TRAINEDDATA ?= $(BIN)/rust/eng.traineddata
ENG_TRAINEDDATA_SOURCE ?=

# The staging directory the release is uploaded FROM.
#
# WHY IT EXISTS: `gh release upload` names each asset by its file BASENAME, so
# the filenames in this directory ARE the public download URLs. Build outputs
# live under bin/rust/ with names that describe how they were built
# (`-static`); consumers must never see that. See the publish target.
PUBLISH := $(BIN)/publish

.PHONY: all rust go windows-rust windows-rust-static rust-tessdata linux-rust linux-rust-static windows-go linux-go publish clean

all: rust go
rust: windows-rust linux-rust
go: windows-go linux-go

# --- Rust: the shipped implementation ---------------------------------------

# Native build. The tesseract/leptonica import libraries come from vcpkg on this
# host, so there is nothing to containerize.
#
# VCPKGRS_DYNAMIC=1 selects the DLL (not static) vcpkg triplet, which is what is
# actually installed; without it the vcpkg crate hunts for static libs that are
# not there and fails at link. The result is DYNAMIC -- running it needs vcpkg's
# installed/x64-windows/bin on PATH.
windows-rust: $(BIN)/rust
	cd ocrd-rust && VCPKGRS_DYNAMIC=1 cargo build --release --locked
	cp ocrd-rust/target/release/ocrd.exe $(BIN)/rust/ocrd-windows-amd64.exe

# Fully static native Windows build. This is intentionally separate from the
# dynamic target above because it needs a different vcpkg triplet and Rust CRT
# mode; mixing those outputs in one Cargo target directory makes it too easy to
# ship whichever link mode happened to build last.
windows-rust-static: $(BIN)/rust
	powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-windows-rust-static.ps1

# Tesseract language data is runtime data, not something the linker can fold
# into a static executable. Stage the default English model beside the release
# assets so Windows installs can set TESSDATA_PREFIX to that directory without
# requiring a separate Tesseract install.
rust-tessdata: $(BIN)/rust
	powershell -NoProfile -ExecutionPolicy Bypass -File scripts/stage-eng-traineddata.ps1 -SourcePath "$(ENG_TRAINEDDATA_SOURCE)" -OutPath "$(ENG_TRAINEDDATA)"

# Built IN a container and copied back out rather than bind-mounted, for two
# reasons: it works against a remote Docker daemon that cannot see this checkout,
# and the musl userland that produces the binary is byte-identical to the one
# that runs it -- leptess links system tesseract, so a libc or libtesseract skew
# between build and run host would surface as a load failure at first use.
#
# Slowest target here by a wide margin (measured 2m49s): bindgen parses the
# tesseract/leptonica headers through libclang, then the release profile's fat
# LTO links the whole program as one codegen unit. Publish from it; never
# develop against it.
linux-rust: $(BIN)/rust
	docker build -f ocrd-rust/Dockerfile.build -t ocrd-rust-build ocrd-rust
	docker rm -f ocrd-rust-extract 2>/dev/null || true
	docker create --name ocrd-rust-extract ocrd-rust-build true
	docker cp ocrd-rust-extract:/out/ocrd $(BIN)/rust/ocrd-linux-amd64
	docker rm ocrd-rust-extract

# Fully static linux/amd64 build. Alpine's binary packages provide dynamic
# libtesseract/libleptonica only, so this target builds those two native
# libraries as static archives inside the container before Cargo links ocrd.
linux-rust-static: $(BIN)/rust
	docker build -f ocrd-rust/Dockerfile.static-linux -t ocrd-rust-static-build ocrd-rust
	docker rm -f ocrd-rust-static-extract 2>/dev/null || true
	docker create --name ocrd-rust-static-extract ocrd-rust-static-build true
	docker cp ocrd-rust-static-extract:/out/ocrd $(BIN)/rust/ocrd-linux-amd64-static
	docker rm ocrd-rust-static-extract

# --- Go: built, never published ---------------------------------------------
#
# Kept building on purpose. An unbuilt second implementation rots, and this one
# is the cheapest check we have that the protocol is implementable from its
# written description rather than only from the Rust source. It is NOT an
# artifact anyone should install -- see the publish target.
windows-go: $(BIN)/go
	cd ocrd-go && CGO_ENABLED=1 GOOS=windows GOARCH=amd64 GOWORK=off \
		go build -o ../$(BIN)/go/ocrd-windows-amd64.exe .

linux-go: $(BIN)/go
	docker build -f ocrd-go/Dockerfile.build -t ocrd-go-build ocrd-go
	docker rm -f ocrd-go-extract 2>/dev/null || true
	docker create --name ocrd-go-extract ocrd-go-build true
	docker cp ocrd-go-extract:/out/ocrd $(BIN)/go/ocrd-linux-amd64
	docker rm ocrd-go-extract

$(BIN)/rust:
	mkdir -p $(BIN)/rust

$(BIN)/go:
	mkdir -p $(BIN)/go

# ONLY THE RUST BINARIES ARE PUBLISHED (owner 2026-08-24: "do not publish the Go
# version"). Consumers get one binary per platform: no choice to make, and no
# question later about which implementation a given machine ended up running.
#
# Depends on the rust targets directly rather than on `all`, so a publish never
# blocks on a Go build and can never ship its output by accident.
#
# THE PUBLISHED NAMES ARE THE CONSUMER'S CONTRACT, AND THEY ARE BUILD-DETAIL
# FREE. What is uploaded is `ocrd-<os>-<arch>` — no language in the name (see
# the preamble at the top of this file) and, for exactly the same reason, no
# LINKAGE in it either. A consumer asks for "the ocrd for linux/amd64"; whether
# that binary was linked statically or dynamically is this repo's business, the
# same as which language it was written in. Baking `-static` into the download
# URL would re-encode a build detail the artifact name deliberately keeps out,
# and would have to be un-baked the next time the linkage changes — a breaking
# rename for every consumer, to describe something no consumer can act on.
#
# `gh release upload` names each asset by its file BASENAME and its `#label`
# syntax sets only the DISPLAY label, so the rename has to happen on disk. The
# artifacts are therefore COPIED into a staging directory under their public
# names. bin/rust/ keeps the `-static` names, because there the distinction is
# real: bin/rust/ holds both linkages at once and the build must not confuse
# them. Copy, not move: a publish must never consume its own inputs.
#
# The staging directory is REBUILT FROM SCRATCH each time. bin/rust/ also holds
# a DYNAMIC ocrd-windows-amd64.exe — a name one character away from the public
# one — and shipping that by accident is the failure this whole target exists to
# avoid: it loads only where vcpkg's DLLs are on PATH, so it would fail at
# rc=127 on every consumer machine and nowhere on the build host.
publish: windows-rust-static linux-rust-static rust-tessdata
	rm -rf $(PUBLISH)
	mkdir -p $(PUBLISH)
	cp $(BIN)/rust/ocrd-windows-amd64-static.exe $(PUBLISH)/ocrd-windows-amd64.exe
	cp $(BIN)/rust/ocrd-linux-amd64-static $(PUBLISH)/ocrd-linux-amd64
	cp $(ENG_TRAINEDDATA) $(PUBLISH)/eng.traineddata
	gh release view $(VERSION) --repo $(REPO) >/dev/null 2>&1 || \
		gh release create $(VERSION) --repo $(REPO) --title "ocrd $(VERSION)" \
			--notes "OCR daemon, windows/amd64 + linux/amd64, plus English Tesseract language data. Protocol and CLI: see README.md."
	gh release upload $(VERSION) --repo $(REPO) --clobber \
		$(PUBLISH)/ocrd-windows-amd64.exe $(PUBLISH)/ocrd-linux-amd64 $(PUBLISH)/eng.traineddata

clean:
	rm -rf $(BIN)/rust $(BIN)/go $(PUBLISH)
