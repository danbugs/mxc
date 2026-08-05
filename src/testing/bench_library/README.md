# MXC Containment Benchmarks

Compares Hyperlight, NanVix (MicroVM), and WSLc backends across cold-start latency, warm-start latency, memory density, and disk footprint.

## Quick start

```powershell
cd src
cargo build --release -p bench_library -p wxc-exec --features hyperlight,microvm,wslc

# Run all benchmarks with hello + compute workloads, output one HTML report
.\target\release\bench-library.exe --full --iterations 10 --warmup 3 --density-count 8 --workloads hello,compute --output-html bench_report.html
```

## What `--full` measures

| Metric | How |
|--------|-----|
| **Cold-start latency** | Spawns `wxc-exec.exe` per iteration, times full process lifetime (creation → teardown). Queries `PeakWorkingSet64` via raw process handle after exit. |
| **Warm-start latency** | Creates runner once via `mxc_engine`, calls `execute()` in a loop. `Instant::now()` around each call; warmup iterations discarded. |
| **Memory density** | Creates N runners, executes each once. Measures per-runner cost differently per backend (see below). Estimates how many fit in 1.5 GB. |
| **Disk footprint** | Measures runtime files. Sparse-aware via `GetCompressedFileSizeW`. |

### Memory model per backend

- **Hyperlight** — VM snapshot loaded in-process via WHP `WHvMapGpaRange`. Each runner holds ~16.8 MB persistently. Measured via `K32GetProcessMemoryInfo` on the bench-library process.

- **NanVix** — Each `execute()` spawns `nanvixd.exe` as a short-lived subprocess (~100ms). VM memory lives in nanvixd, not bench-library. Measured by polling `nanvixd.exe` WS via `CreateToolhelp32Snapshot` + `K32GetProcessMemoryInfo` during execution at 1ms intervals.

- **WSLc** — Each `execute()` creates a container via `wslservice.exe` (persistent system service, ~38 MB baseline). Measured as wslservice WS delta + bench-library client overhead. **Note:** container memory lives in the WSL2 VM and is not visible in host process WS — reported numbers capture Windows-side overhead only.

### Disk footprint per backend

- **Hyperlight** — OCI snapshot at `%LOCALAPPDATA%\pyhl\snapshot\` (sparse-aware). Installed by `wxc-exec --setup-hyperlight`.
- **NanVix** — `nanvixd.exe`, `nanvix_rootfs.img`, `python3.initrd`, `kernel.elf` next to `wxc-exec.exe`.
- **WSLc** — OCI images stored inside shared WSL2 ext4.vhdx. Per-image size not measurable from host.

## Individual benchmarks

```powershell
# Warm-start only
.\target\release\bench-library.exe --all --iterations 20 --warmup 5

# Density only
.\target\release\bench-library.exe --density 8 --all

# Compute workload (fibonacci instead of hello-world)
.\target\release\bench-library.exe --all --workload compute --iterations 10

# JSON output
.\target\release\bench-library.exe --all --output-json results.json
```

## Adding a backend

1. Add CLI string mapping in `parse_backend()`
2. Add a `make_request()` arm for the new `ContainmentBackend` variant
3. If the backend uses an external daemon, add its process name in `daemon_process_names()`
4. Add disk file list in `measure_disk()`
5. Enable the feature in `Cargo.toml`
