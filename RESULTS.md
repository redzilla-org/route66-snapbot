# Current results

Measured on a dual-socket Windows 11 x86-64 host with two
`Intel(R) Xeon(R) Gold 6138 CPU @ 2.00GHz` packages: 40 physical cores and 80
logical processors total. Each benchmark process is single-threaded.

These are live results. Rank is determined by the fastest valid observed
end-to-end total across repeated sessions, not by a median.

## Ranking method

Each process reads its PNG input once, performs 3 warmups, and then collects
complete PNG-to-PGM iterations for a 250 ms timed window. The process reports
`total.min`, the fastest complete iteration in that window. Every session runs
all contenders in shuffled order five times. The headline value is the fastest
valid `total.min` observed for that contender across repeated sessions.

Medians, pair signs, and ranges remain useful noise diagnostics, but they do not
rank contenders and are not used in the recommendation. Pixel correctness is
checked after timing. The production objective is:

```text
weighted latency = 0.8 * typical total + 0.2 * big total
```

## Full field

| rank | pipeline | typical total | big total | 80/20 weighted | eligibility |
|---:|---|---:|---:|---:|---|
| **1** | **Rust `png` 0.18.1/fdeflate + KSTREAM, non-PGO** | 28.43 ms | **54.11 ms** | **33.57 ms** | eligible |
| 2 | Rust `png` 0.18.1/fdeflate + KSTREAM, 80/20 PGO | 29.17 ms | 55.18 ms | 34.37 ms | eligible |
| 3 | Go custom PNG + GOFAST | **27.59 ms** | 69.99 ms | 36.07 ms | eligible |
| 4 | Go 1.27 custom PNG + API SIMD GO127 | 38.32 ms | 79.38 ms | 46.53 ms | eligible |
| 5 | Wuffs decode + C++ KLUT | 50.24 ms | 74.64 ms | 55.12 ms | eligible |
| 6 | Zig `zignal` + KLUT | 84.08 ms | 148.74 ms | 97.01 ms | eligible |
| 7 | Nim `nimPNG` + KLUT | 90.08 ms | 228.21 ms | 117.70 ms | eligible |
| - | Python Pillow | 154.75 ms | 115.07 ms | 146.81 ms | ineligible quality |

Go `GOFAST` owns the fastest `typical` observation. Pure Rust `KSTREAM` wins `big`
and the weighted production objective. The 80/20-trained PGO build is slightly
slower on both fixtures, so the simpler non-PGO native build is the recommendation.
No phase values are used to construct these totals; every number is a measured
complete PNG-to-PGM iteration.

## Winning Rust pipeline

`KSTREAM` uses `png` 0.18.1 with fdeflate. Its trusted-input decoder options skip
PNG checksum validation and metadata processing. Decoded RGB/RGBA is converted to
a retained 8.8 fixed-point luma plane while global minimum and maximum are collected.
Factor-2 scaling streams through two luma row buffers instead of allocating a full
scaled intermediate; factor-1 output is materialized directly.

The non-PGO release build uses native CPU code generation and fat LTO. The PGO
contender instruments the complete release dependency graph and trains KSTREAM with
four `typical` runs and one `big` run, matching the declared 80/20 workload. Its
34.37 ms weighted result does not improve on native Rust's 33.57 ms.

## Quality

The eligibility gate permits a maximum difference of one gray level on at most
1% of output pixels.

| output group | typical difference | big difference | maximum delta | status |
|---|---:|---:|---:|---|
| Rust native, Rust PGO, Go GOFAST, Go127 | 0 / 15,927,560 | 0 / 7,481,582 | 0 | eligible |
| Zig, Nim, Wuffs/C++, HYBRID | 44,641 / 15,927,560 (0.2803%) | 24,579 / 7,481,582 (0.3285%) | 1 | eligible |
| Python Pillow | 244,195 / 15,927,560 (1.5332%) | 24,669 / 7,481,582 (0.3297%) | 2 typical, 1 big | ineligible |

Rust native and PGO, Go GOFAST, and Go127 are byte-identical on both fixtures.
The KLUT-based Zig, Nim, Wuffs/C++, and hybrid outputs remain inside the accepted
one-gray-level tolerance. Pillow exceeds both the 1% rate and one-level maximum on
`typical`, so its timing cannot qualify regardless of its `big` result.

## Non-pure pipeline

The hybrid keeps Rust PNG decode and PGM write but uses the C++ transform:

| pipeline | typical total | big total | 80/20 weighted | status |
|---|---:|---:|---:|---|
| Rust decode / C++ transform / Rust write `HYBRID` | 41.50 ms | 63.21 ms | 45.84 ms | eligible, non-pure |

The hybrid is slower than pure Rust by 12.27 ms on the weighted objective. It is
reported separately because decode, transform, and write do not share one language.

## Reproduction

Build the native winner and its PGO comparison with the same CPU specialization:

```powershell
$env:RUSTFLAGS = "-C target-cpu=native"
cargo build --release --manifest-path rust\Cargo.toml
Copy-Item rust\target\release\rsbench.exe rust\bin\rsbench_nativefat.exe

rustup component add llvm-tools-preview
.\rust\build_pgo.ps1 -ProfileVariant KSTREAM
```

Run all eight full-field contestants under the ranking protocol:

```powershell
python prof\paired_lang.py typical 5 rs:nativefat:KSTREAM `
  rs:kstreampgo:KSTREAM go:fastest_v3:GOFAST go127:GO127 `
  wf:KLUT zig:KLUT nim:KLUT py:PIL

python prof\paired_lang.py big 5 rs:nativefat:KSTREAM `
  rs:kstreampgo:KSTREAM go:fastest_v3:GOFAST go127:GO127 `
  wf:KLUT zig:KLUT nim:KLUT py:PIL
```

Run the non-pure contender separately:

```powershell
python prof\paired_lang.py typical 5 rs:nativefat:KSTREAM rs:nativefat:HYBRID
python prof\paired_lang.py big 5 rs:nativefat:KSTREAM rs:nativefat:HYBRID
```

The retained raw sessions are stored in `results/`. The headline values are derived
from `results/full_typical_session1.json`, `full_typical_session2.json`,
`full_big_session1.json`, and `full_big_session2.json`; the calculation is retained
in `results/fastest_summary.json`. Hybrid sessions are retained beside them.

## Recommendation

Use non-PGO Rust `KSTREAM`. Go is fastest on `typical`, but Rust wins `big` and has
the lowest 80/20 weighted latency, 33.57 ms. PGO, GOFAST, GO127, Wuffs/C++, Zig,
Nim, Pillow, and the Rust/C++ hybrid do not improve that objective.
