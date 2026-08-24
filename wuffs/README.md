# Wuffs benchmark dependency

Wuffs is an official Git submodule at `upstream`, pinned by the parent repository to
v0.3.5 commit `d1a6f2c3de7e52b8775782028e5a49946a7f9184`.

Initialize and build it from the repository root:

```powershell
# The benchmark compiles Wuffs' official generated C release through its C++ API.
git submodule update --init wuffs/upstream
./wuffs/build.ps1
```

The deployed decoder artifact is
[`upstream/release/c/wuffs-v0.3.c`](upstream/release/c/wuffs-v0.3.c). The corresponding
hand-written Wuffs-language source is directly available in the submodule:

- [`upstream/std/png`](upstream/std/png) — PNG decoder, filters and pixel swizzling.
- [`upstream/std/deflate`](upstream/std/deflate) — DEFLATE and Huffman decoding.
- [`upstream/std/zlib`](upstream/std/zlib) — zlib wrapper.
- [`upstream/std/adler32`](upstream/std/adler32) and
  [`upstream/std/crc32`](upstream/std/crc32) — checksums.

The upstream Apache 2.0 license is at [`upstream/LICENSE`](upstream/LICENSE).
