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

.PHONY: all rust go windows-rust linux-rust windows-go linux-go publish clean

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
publish: rust
	gh release view $(VERSION) --repo $(REPO) >/dev/null 2>&1 || \
		gh release create $(VERSION) --repo $(REPO) --title "ocrd $(VERSION)" \
			--notes "OCR daemon, windows/amd64 + linux/amd64. Protocol and CLI: see README.md."
	gh release upload $(VERSION) --repo $(REPO) --clobber \
		$(BIN)/rust/ocrd-windows-amd64.exe $(BIN)/rust/ocrd-linux-amd64

clean:
	rm -rf $(BIN)/rust $(BIN)/go
