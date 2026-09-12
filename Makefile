# Snapbot has one deployable artifact: its Lambda image. Both targets route
# through Dockerfiles checked into this repository so no host toolchain becomes
# an undocumented prerequisite.
.PHONY: build test

build:
	docker build --platform linux/amd64 -t route66-snapbot:local .

test:
	docker build --platform linux/amd64 -f Dockerfile.test -t route66-snapbot:test .
