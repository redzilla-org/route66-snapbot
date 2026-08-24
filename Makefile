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
REPO    ?= refacktor/ocr-daemon
BIN     := bin

.PHONY: all windows linux publish clean

all: windows linux

windows: $(BIN)
	cd ocrd-go && CGO_ENABLED=1 GOOS=windows GOARCH=amd64 GOWORK=off \
		go build -o ../$(BIN)/ocrd-go-windows-amd64.exe .

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
publish: all
	gh release view $(VERSION) --repo $(REPO) >/dev/null 2>&1 || \
		gh release create $(VERSION) --repo $(REPO) --title "ocrd $(VERSION)" \
			--notes "Go OCR daemon binaries (windows/amd64, linux/amd64). Protocol: see ocrd-rust/README.md."
	gh release upload $(VERSION) --repo $(REPO) --clobber \
		$(BIN)/ocrd-go-windows-amd64.exe $(BIN)/ocrd-go-linux-amd64

clean:
	rm -rf $(BIN)
