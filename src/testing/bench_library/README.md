# MXC Containment Benchmarks

Benchmark suite for comparing MXC containment backends. Two complementary tools:

- **`bench-library`** — Rust binary that links `mxc_engine` directly. Measures steady-state per-invocation latency with runner reuse (no process overhead).
- **`bench_containments.ps1`** — PowerShell harness that invokes `wxc-exec.exe` as a child process. Measures end-to-end latency including process creation, config parsing, runner construction, and teardown. Also collects per-VM peak working set and on-disk footprint.

## What each mode measures

| Metric | Library mode (`bench-library`) | CLI mode (`bench_containments.ps1`) |
|--------|-------------------------------|-------------------------------------|
| **Scope** | `runner.execute()` call only | Full `wxc-exec.exe` process lifetime |
| **Runner lifecycle** | Created once, reused across iterations | New process → new runner per iteration |
| **What it isolates** | Per-invocation cost (script execution + VM round-trip) | Cold-start cost (process + config + runner creation + execution) |
| **Memory** | Not measured (single process) | Peak working set per wxc-exec invocation |
| **Disk** | Not measured | Snapshot/rootfs/image sizes (sparse-aware for NTFS) |

## Quick start

### Prerequisites

- Windows with WHP enabled (Hyperlight, NanVix) and/or WSL (WSLc)
- `wxc-exec.exe` built in release mode: `cargo build --release -p wxc-exec --features hyperlight,microvm,wslc`
- Hyperlight snapshot set up: `wxc-exec.exe --setup-hyperlight`
- NanVix daemon running: `nanvixd.exe`
- For WSLc: a Linux container image available (e.g., `python:3.12-alpine`)

### Library mode

```powershell
# Build
cargo build --release -p bench_library

# Run all backends (20 iterations, 5 warmup)
.\src\target\release\bench-library.exe --all --iterations 20 --warmup 5

# Single backend with HTML output
.\src\target\release\bench-library.exe --backend hyperlight --output-html report.html

# With custom WSLc image
.\src\target\release\bench-library.exe --all --wslc-image python:3.12-alpine --output-json results.json
```

### CLI mode

```powershell
# All backends, 10 iterations, JSON + HTML output
.\scripts\bench_containments.ps1 -Backends hyperlight,microvm,wslc -Iterations 10 `
    -OutputJson results.json -OutputHtml report.html

# Single backend
.\scripts\bench_containments.ps1 -Backends hyperlight -Iterations 20

# Skip setup (if snapshot/daemon already ready)
.\scripts\bench_containments.ps1 -SkipSetup -Iterations 10
```

## Adding a new backend

### Library mode (`bench-library`)

The binary dispatches via `mxc_engine::resolve_runner()`. To add a backend:

1. Add the CLI string mapping in `main()` (the `--backend` match)
2. Add a `make_request()` arm to build the correct `ExecutionRequest` (set `containment`, `script_code`, and any experimental config)
3. Enable the feature in `Cargo.toml` (e.g., `mxc_engine = { ..., features = ["isolation_session"] }`)

The `ContainmentBackend` enum has these variants available on Windows:
- `ProcessContainer` — default Windows sandbox (AppContainer/BaseContainer)
- `Hyperlight` — Unikraft unikernel via WHP (experimental)
- `MicroVm` — NanVix microkernel via WHP (experimental)
- `Wslc` — Linux container via WSL Container SDK (experimental)
- `WindowsSandbox` — full Windows Sandbox VM (experimental)
- `IsolationSession` — IsoEnvBroker session API (experimental, requires `--features isolation_session`)

### CLI mode (`bench_containments.ps1`)

1. Add a workload config JSON file at `tests/bench/workloads/{workload}_{backend}.json`
2. Add the backend name to the `-Backends` parameter validation
3. Add any setup logic in the setup section (if the backend needs pre-flight)

## Architecture

```
bench-library (Rust)
├── CLI parsing (clap)
├── make_request(backend) → ExecutionRequest
├── benchmark_backend()
│   ├── resolve_runner() → Box<dyn ScriptRunner>  (once)
│   ├── warmup loop (discarded)
│   └── timed loop: Instant → runner.execute() → elapsed
├── compute stats (min/median/mean/p95/max/stdev)
└── output: JSON (stdout/file) + HTML (file)

bench_containments.ps1 (PowerShell)
├── Setup (snapshot restore, daemon start)
├── Invoke-Iteration()
│   ├── ProcessStartInfo → CreateNoWindow, redirect stdout/stderr
│   ├── Stopwatch around process lifetime
│   ├── Poll PeakWorkingSet64 while running
│   └── Parse restore/call sub-timings from log output
├── Measure-BackendFootprint()
│   ├── Hyperlight: fsutil file layout → actual sparse allocation
│   ├── NanVix: rootfs + initrd + kernel + daemon binary
│   └── WSLc: OCI image cache size
├── Compute stats (min/median/mean/p90/p99/max/stdev)
└── Output: console table + JSON + HTML with Canvas charts
```

## Output formats

Both tools produce:
- **JSON** — machine-readable results with per-iteration timings and aggregate stats
- **HTML** — self-contained report with Canvas charts (strip plot, per-iteration line chart, summary table)
- **Console** — formatted summary table (PS1 only)
