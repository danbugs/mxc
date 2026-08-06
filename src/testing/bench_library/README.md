# MXC Containment Benchmarks

Compares Hyperlight, NanVix (MicroVM), and WSLc backends across cold-start latency, warm-start latency, memory density (commit charge), parallel throughput, and disk footprint.

## Quick start

```powershell
cd src
cargo build --release -p bench_library -p wxc-exec --features hyperlight,microvm,wslc

# Run all benchmarks with hello + compute workloads, output one HTML report
.\target\release\bench-library.exe --full --iterations 10 --warmup 3 --density-count 8 --parallel-count 5 --workloads hello,compute --output-html bench_report.html
```

## What `--full` measures

| Metric | How |
|--------|-----|
| **Cold-start latency** | Spawns `wxc-exec.exe` per iteration, times full process lifetime (creation → teardown). Queries peak commit charge via raw process handle after exit. |
| **Warm-start latency** | Creates runner once via `mxc_engine`, calls `execute()` in a loop. `Instant::now()` around each call; warmup iterations discarded. |
| **Memory density** | Creates N runners, executes each once. Measures per-runner memory commit charge (not working set) per backend (see below). Estimates how many fit in 1.5 GB. |
| **Parallel throughput** | Creates N concurrent runners (default 5), synchronizes via barrier, executes workload simultaneously. Measures wall-clock time, per-runner latency under contention, and throughput (exec/sec). |
| **Disk footprint** | Measures runtime files. Sparse-aware via `GetCompressedFileSizeW`. |

### Why each backend is measured differently

Each backend has a different memory architecture, so we measure where the VM/container memory actually lives:

- **Hyperlight** — VM snapshot loaded in-process via WHP `WHvMapGpaRange`. Each runner's memory lives in the bench-library process. Measured via per-process commit charge (`K32GetProcessMemoryInfo` → `PagefileUsage`).

- **NanVix** — Each `execute()` spawns `nanvixd.exe` as a short-lived subprocess. The full VM address space (~258 MB) is committed in nanvixd, not bench-library. Measured by polling nanvixd's commit charge via `CreateToolhelp32Snapshot` + `K32GetProcessMemoryInfo` during execution.

- **WSLc** — Each `execute()` creates a container inside the shared WSL2 VM. Container memory is invisible to host process APIs — it lives in the `vmmem` kernel pseudo-process. Measured from inside the container via Linux cgroup stats (`/sys/fs/cgroup/memory.current`).

We use commit charge (not working set) everywhere because it represents virtual memory backed by physical RAM or pagefile — stable under memory pressure, unlike working set which the OS can trim.

### Disk footprint per backend

- **Hyperlight** — OCI snapshot at `%LOCALAPPDATA%\pyhl\snapshot\` (sparse-aware). Installed by `wxc-exec --setup-hyperlight`.
- **NanVix** — `nanvixd.exe`, `nanvix_rootfs.img`, `python3.initrd`, `kernel.elf` next to `wxc-exec.exe`.
- **WSLc** — Core WSL2 runtime (`C:\Program Files\WSL\`: `system.vhd`, `wslservice.exe`, `container.exe`, etc.) plus OCI image rootfs measured from inside a temporary container via `du -sx /`.

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
