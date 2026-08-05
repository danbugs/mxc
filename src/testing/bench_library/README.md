# MXC Containment Benchmarks

Two tools for comparing Hyperlight, NanVix (MicroVM), and WSLc.

## Tools

**`bench-library`** (Rust) — Links `mxc_engine` directly. Creates runner once, calls `execute()` in a loop. Measures warm-start latency and per-runner memory density.

**`bench_containments.ps1`** (PowerShell) — Spawns `wxc-exec.exe` per iteration. Measures cold-start latency, per-process peak WS, and on-disk footprint.

## What each tool measures

| Metric | `bench-library` | `bench_containments.ps1` |
|--------|-----------------|--------------------------|
| Latency scope | `runner.execute()` only | Full `wxc-exec.exe` process lifetime |
| Runner lifecycle | Created once, reused | New process per iteration |
| Memory | `--density N`: per-runner WS + daemon WS | Peak WS per wxc-exec invocation |
| Disk | — | Snapshot/rootfs sizes (sparse-aware) |

## Measurement methodology

### Latency

- **Cold-start** (CLI): Wall-clock time of `wxc-exec.exe` process from `Start` to `WaitForExit`. Includes process creation, config parsing, runner construction, VM boot, script execution, teardown. Timed via `System.Diagnostics.Stopwatch`, process launched with raw `ProcessStartInfo` (not `Start-Process`, which adds ~250ms console allocation overhead).

- **Warm-start** (Library): `Instant::now()` around `runner.execute()`. Runner is pre-created and reused. Warmup iterations are discarded.

### Memory (density)

Memory measurement varies by backend architecture:

- **Hyperlight** — VM snapshot is loaded in-process via WHP `WHvMapGpaRange`. Each runner holds ~16.8 MB of snapshot memory persistently. Measured via `K32GetProcessMemoryInfo` on the bench-library process.

- **NanVix** — Each `execute()` spawns `nanvixd.exe` as a short-lived subprocess (~100ms). The VM memory lives in nanvixd, not in bench-library. Measured by polling `nanvixd.exe` WS via `CreateToolhelp32Snapshot` + `K32GetProcessMemoryInfo` during execution. Peak WS ~11 MB per VM.

- **WSLc** — Each `execute()` creates a container via `wslservice.exe` (persistent system service). Measured by capturing wslservice WS delta (peak during execution minus baseline). Per-container delta ~0.2 MB; bench-library client overhead ~1.3 MB/exec.

### Disk footprint

Measured by the PS1 script. Hyperlight uses NTFS sparse files — actual on-disk allocation is measured via `fsutil file layout` (not logical file size).

## Quick start

```powershell
# Build
cd src
cargo build --release -p bench_library -p wxc-exec --features hyperlight,microvm,wslc

# Warm-start latency (all backends, 20 iterations)
.\target\release\bench-library.exe --all --iterations 20 --warmup 5

# Density test (8 runners)
.\target\release\bench-library.exe --density 8 --all

# Cold-start latency + disk footprint
.\scripts\bench_containments.ps1 -Backends hyperlight,microvm,wslc -Iterations 10

# Compute workload (~150ms CPU fibonacci instead of hello-world)
.\target\release\bench-library.exe --all --workload compute --iterations 10

# JSON output
.\target\release\bench-library.exe --all --output-json results.json
.\target\release\bench-library.exe --density 8 --all --output-json density.json
```

## Adding a backend

1. Add the CLI string mapping in `parse_backend()`
2. Add a `make_request()` arm for the new `ContainmentBackend` variant
3. If the backend uses an external daemon, add its process name in `daemon_process_names()`
4. Enable the feature in `Cargo.toml`
