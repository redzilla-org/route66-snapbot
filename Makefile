# Builds the Go OCR daemon (ocrd-go) for both platforms and publishes the
# binaries to GitHub releases. The Rust daemon (ocrd-rust) is kept in-repo as
# the reference implementation but is NOT built here -- the Go build is the
# one consumers download (see BUILD_TIMES.md for why).
#
# Windows build needs (already true on the reference workstation):
#   - mingw-w64 gcc on PATH (cgo)
#   - vcpkg tesseract:x64-windows at C:/Users/alexr/vcpkg (paths in tess.go)
# Linux build needs only a reachable docker daemon: the compile runs inside a
# golang:alpine image so no cross-toolchain is installed on the host.

VERSION ?= v0.1.0
REPO    ?= redzilla-org/ocr-daemon
BIN     := bin

.PHONY: all windows windows-rust linux linux-rust publish clean

all: windows windows-rust linux linux-rust

windows: $(BIN)
	cd ocrd-go && CGO_ENABLED=1 GOOS=windows GOARCH=amd64 GOWORK=off \
		go build -o ../$(BIN)/ocrd-go-windows-amd64.exe .

# The Rust daemon, windows/amd64. Native build -- no container, because the
# tesseract/leptonica import libraries come from vcpkg on this host.
#
# VCPKGRS_DYNAMIC=1 selects the DLL (not static) vcpkg triplet, which is what
# is actually installed; without it the vcpkg crate looks for static libs that
# are not there and fails at link. The produced binary is DYNAMIC: running it
# needs vcpkg's installed/x64-windows/bin on PATH for the DLLs.
windows-rust: $(BIN)
	cd ocrd-rust && VCPKGRS_DYNAMIC=1 cargo build --release --locked
	cp ocrd-rust/target/release/ocrd.exe $(BIN)/ocrd-rust-windows-amd64.exe

# Remote-daemon-safe: build context is uploaded, binary comes back via cp.
linux: $(BIN)
	docker build -f ocrd-go/Dockerfile.build -t ocrd-go-build ocrd-go
	docker rm -f ocrd-go-extract 2>/dev/null || true
	docker create --name ocrd-go-extract ocrd-go-build true
	docker cp ocrd-go-extract:/out/ocrd-go-linux-amd64 $(BIN)/ocrd-go-linux-amd64
	docker rm ocrd-go-extract

$(BIN):
	mkdir -p $(BIN)

# Creates the release if absent, then uploads/replaces both binaries.
# The Rust daemon, linux/amd64. Same remote-daemon-safe shape as the Go linux
# target: the context is uploaded and the artifact comes back via cp, because a
# bind mount cannot reach a Windows checkout from a remote Linux dockerd.
#
# Slower than every other target here by a wide margin -- bindgen parses the
# tesseract/leptonica headers through libclang, then the release profile's
# fat LTO with a single codegen unit links the whole program at once. That cost
# is per-build, not per-edit; iterate with a dev profile.
linux-rust: $(BIN)
	docker build -f ocrd-rust/Dockerfile.build -t ocrd-rust-build ocrd-rust
	docker rm -f ocrd-rust-extract 2>/dev/null || true
	docker create --name ocrd-rust-extract ocrd-rust-build true
	docker cp ocrd-rust-extract:/out/ocrd-rust-linux-amd64 $(BIN)/ocrd-rust-linux-amd64
	docker rm ocrd-rust-extract

publish: all
	gh release view $(VERSION) --repo $(REPO) >/dev/null 2>&1 || \
		gh release create $(VERSION) --repo $(REPO) --title "ocrd $(VERSION)" \
			--notes "Go OCR daemon binaries (windows/amd64, linux/amd64). Protocol: see ocrd-rust/README.md."
	gh release upload $(VERSION) --repo $(REPO) --clobber \
		$(BIN)/ocrd-go-windows-amd64.exe $(BIN)/ocrd-go-linux-amd64 		$(BIN)/ocrd-rust-linux-amd64 $(BIN)/ocrd-rust-windows-amd64.exe

clean:
	rm -rf $(BIN)
