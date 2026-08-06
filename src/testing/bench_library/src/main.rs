// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Library-mode benchmark for MXC containment backends.
//!
//! Measures steady-state per-invocation latency, per-runner memory density,
//! parallel throughput, and disk footprint. Each backend has a different
//! memory architecture, so density is measured where the memory actually lives:
//!   - Hyperlight: in-process commit (VM snapshot mapped via WHP)
//!   - NanVix:     subprocess commit (nanvixd holds the full VM address space)
//!   - WSLc:       cgroup memory from inside the container (in the WSL2 VM)
//!
//! Usage:
//!   bench-library --backend hyperlight --iterations 20 --warmup 3
//!   bench-library --all --iterations 10 --output-json results.json
//!   bench-library --all --workload compute --iterations 10
//!   bench-library --density 8 --all

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use serde::Serialize;
use wxc_common::config_parser::load_request;
use wxc_common::logger::{Logger, Mode};
use wxc_common::models::{ContainmentBackend, ExecutionRequest, ScriptResponse};
use wxc_common::script_runner::ScriptRunner;

#[derive(Parser)]
#[command(
    name = "bench-library",
    about = "Library-mode benchmark for MXC containment backends"
)]
struct Cli {
    /// Which backend to benchmark (hyperlight, microvm, wslc).
    /// Ignored when --all is set.
    #[arg(long, value_parser = parse_backend)]
    backend: Option<ContainmentBackend>,

    /// Benchmark all available backends sequentially.
    #[arg(long)]
    all: bool,

    /// Number of timed iterations per backend.
    #[arg(long, default_value = "20")]
    iterations: usize,

    /// Number of warmup iterations (not included in stats).
    #[arg(long, default_value = "3")]
    warmup: usize,

    /// Path to a workload config JSON (overrides the built-in hello world).
    #[arg(long)]
    config: Option<String>,

    /// Workload to run: "hello" (trivial print) or "compute" (~150ms CPU work).
    /// Ignored when --config or --workloads is set.
    #[arg(long, default_value = "hello")]
    workload: String,

    /// Comma-separated list of workloads for --full mode (e.g. "hello,compute").
    /// Runs all benchmarks for each workload; the HTML report gets tabs.
    #[arg(long, value_delimiter = ',')]
    workloads: Option<Vec<String>>,

    /// WSLc container image (default: python:3.12-alpine).
    #[arg(long, default_value = "python:3.12-alpine")]
    wslc_image: String,

    /// Write results to a JSON file.
    #[arg(long)]
    output_json: Option<String>,

    /// Write results as an HTML report.
    #[arg(long)]
    output_html: Option<String>,

    /// Density test: create N runners simultaneously and measure process memory.
    /// Mutually exclusive with normal benchmark mode.
    #[arg(long)]
    density: Option<usize>,

    /// Run all benchmarks (cold-start, warm-start, density, disk) and produce
    /// a unified HTML report. Implies --all. Use --output-html to set path.
    #[arg(long)]
    full: bool,

    /// Number of density runners for --full mode (default: 8).
    #[arg(long, default_value = "8")]
    density_count: usize,

    /// Number of concurrent runners for the parallel benchmark (default: 5).
    #[arg(long, default_value = "5")]
    parallel_count: usize,
}

fn parse_backend(s: &str) -> Result<ContainmentBackend, String> {
    match s.to_lowercase().as_str() {
        "hyperlight" => Ok(ContainmentBackend::Hyperlight),
        "microvm" | "nanvix" => Ok(ContainmentBackend::MicroVm),
        "wslc" => Ok(ContainmentBackend::Wslc),
        other => Err(format!(
            "unknown backend '{other}'; expected: hyperlight, microvm, wslc"
        )),
    }
}

fn backend_display_name(b: &ContainmentBackend) -> &'static str {
    match b {
        ContainmentBackend::Hyperlight => "Hyperlight",
        ContainmentBackend::MicroVm => "NanVix (MicroVM)",
        ContainmentBackend::Wslc => "WSLc",
        _ => "Unknown",
    }
}

/// Returns the external daemon process names for a given backend.
fn daemon_process_names(b: &ContainmentBackend) -> Vec<&'static str> {
    match b {
        ContainmentBackend::MicroVm => vec!["nanvixd"],
        ContainmentBackend::Wslc => vec!["wslservice"],
        _ => vec![],
    }
}

/// Whether this backend's VM/container memory is ephemeral (freed after execute).
fn is_ephemeral_backend(b: &ContainmentBackend) -> bool {
    matches!(
        b,
        ContainmentBackend::MicroVm | ContainmentBackend::Wslc
    )
}

// ---------------------------------------------------------------------------
// Process memory measurement (cross-platform, no external crates)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod mem_win {
    use std::mem;

    #[repr(C)]
    #[allow(non_snake_case)]
    pub struct ProcessMemoryCounters {
        pub cb: u32,
        pub PageFaultCount: u32,
        pub PeakWorkingSetSize: usize,
        pub WorkingSetSize: usize,
        pub QuotaPeakPagedPoolUsage: usize,
        pub QuotaPagedPoolUsage: usize,
        pub QuotaPeakNonPagedPoolUsage: usize,
        pub QuotaNonPagedPoolUsage: usize,
        pub PagefileUsage: usize,
        pub PeakPagefileUsage: usize,
    }

    const TH32CS_SNAPPROCESS: u32 = 0x00000002;
    const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
    const PROCESS_VM_READ: u32 = 0x0010;
    const INVALID_HANDLE_VALUE: isize = -1;
    const MAX_PATH: usize = 260;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESSENTRY32W {
        dwSize: u32,
        cntUsage: u32,
        th32ProcessID: u32,
        th32DefaultHeapID: usize,
        th32ModuleID: u32,
        cntThreads: u32,
        th32ParentProcessID: u32,
        pcPriClassBase: i32,
        dwFlags: u32,
        szExeFile: [u16; MAX_PATH],
    }

    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            ppsmem_counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> isize;
        fn Process32FirstW(hSnapshot: isize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn Process32NextW(hSnapshot: isize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn CloseHandle(hObject: isize) -> i32;
    }

    /// Returns the current process memory commit charge (private bytes) in MB.
    /// Commit = virtual memory backed by pagefile or physical RAM, regardless
    /// of whether pages are currently resident. More stable than WS.
    pub fn process_commit_mb() -> f64 {
        unsafe {
            let handle = GetCurrentProcess();
            let mut c: ProcessMemoryCounters = mem::zeroed();
            c.cb = mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut c, c.cb) != 0 {
                c.PagefileUsage as f64 / (1024.0 * 1024.0)
            } else {
                0.0
            }
        }
    }

    /// Returns the total commit charge (MB) of all processes matching any given name.
    pub fn external_process_commit_mb(target_names: &[&str]) -> f64 {
        use std::os::windows::ffi::OsStringExt;

        if target_names.is_empty() {
            return 0.0;
        }

        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return 0.0;
            }

            let mut entry: PROCESSENTRY32W = mem::zeroed();
            entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as u32;

            let mut total: f64 = 0.0;

            if Process32FirstW(snap, &mut entry) != 0 {
                loop {
                    let name_len = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(MAX_PATH);
                    let exe_name = std::ffi::OsString::from_wide(&entry.szExeFile[..name_len])
                        .to_string_lossy()
                        .to_lowercase();

                    let matches = target_names.iter().any(|t| {
                        let target = t.to_lowercase();
                        exe_name == target || exe_name == format!("{}.exe", target)
                    });

                    if matches {
                        let proc_handle = OpenProcess(
                            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                            0,
                            entry.th32ProcessID,
                        );
                        if proc_handle != 0 {
                            let mut c: ProcessMemoryCounters = mem::zeroed();
                            c.cb = mem::size_of::<ProcessMemoryCounters>() as u32;
                            if K32GetProcessMemoryInfo(proc_handle, &mut c, c.cb) != 0 {
                                total += c.PagefileUsage as f64;
                            }
                            CloseHandle(proc_handle);
                        }
                    }

                    if Process32NextW(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }

            CloseHandle(snap);
            total / (1024.0 * 1024.0)
        }
    }

    /// Returns the peak commit charge (MB) of a process given its raw HANDLE.
    pub fn process_peak_commit_by_handle(handle: isize) -> f64 {
        unsafe {
            let mut c: ProcessMemoryCounters = mem::zeroed();
            c.cb = mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut c, c.cb) != 0 {
                c.PeakPagefileUsage as f64 / (1024.0 * 1024.0)
            } else {
                0.0
            }
        }
    }

    /// Returns system-wide commit charge used (MB) via GlobalMemoryStatusEx.
    /// Captures ALL memory including VM memory (vmmem) that per-process APIs can't see.
    pub fn system_commit_used_mb() -> f64 {
        #[repr(C)]
        #[allow(non_snake_case)]
        struct MEMORYSTATUSEX {
            dwLength: u32,
            dwMemoryLoad: u32,
            ullTotalPhys: u64,
            ullAvailPhys: u64,
            ullTotalPageFile: u64,
            ullAvailPageFile: u64,
            ullTotalVirtual: u64,
            ullAvailVirtual: u64,
            ullAvailExtendedVirtual: u64,
        }

        extern "system" {
            fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
        }

        unsafe {
            let mut status: MEMORYSTATUSEX = mem::zeroed();
            status.dwLength = mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 {
                (status.ullTotalPageFile - status.ullAvailPageFile) as f64 / (1024.0 * 1024.0)
            } else {
                0.0
            }
        }
    }

    /// Returns the actual on-disk allocation of a file (handles NTFS sparse/compressed).
    pub fn file_actual_size_mb(path: &std::path::Path) -> f64 {
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn GetCompressedFileSizeW(lpFileName: *const u16, lpFileSizeHigh: *mut u32) -> u32;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut high: u32 = 0;
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        if low == 0xFFFFFFFF && std::io::Error::last_os_error().raw_os_error() != Some(0) {
            // GetCompressedFileSize failed — fall back to logical size
            path.metadata().map(|m| m.len() as f64 / (1024.0 * 1024.0)).unwrap_or(0.0)
        } else {
            let size = ((high as u64) << 32) | (low as u64);
            size as f64 / (1024.0 * 1024.0)
        }
    }
}

#[cfg(target_os = "windows")]
use mem_win::{
    external_process_commit_mb, file_actual_size_mb,
    process_commit_mb, process_peak_commit_by_handle,
    system_commit_used_mb,
};

#[cfg(not(target_os = "windows"))]
fn process_commit_mb() -> f64 {
    // Best-effort fallback: read VmRSS from /proc/self/status
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                if let Some(kb_str) = line.split_whitespace().nth(1) {
                    if let Ok(kb) = kb_str.parse::<f64>() {
                        return kb / 1024.0;
                    }
                }
            }
        }
    }
    0.0
}

#[cfg(not(target_os = "windows"))]
fn external_process_commit_mb(_target_names: &[&str]) -> f64 {
    0.0
}

#[cfg(not(target_os = "windows"))]
fn process_peak_commit_by_handle(_handle: isize) -> f64 {
    0.0
}

#[cfg(not(target_os = "windows"))]
fn system_commit_used_mb() -> f64 {
    0.0
}

// ---------------------------------------------------------------------------
// Timing and stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct IterationResult {
    iteration: usize,
    elapsed_ms: f64,
    exit_code: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    stdout_preview: String,
}

#[derive(Debug, Clone, Serialize)]
struct Stats {
    count: usize,
    min_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    median_ms: f64,
    p95_ms: f64,
    stdev_ms: f64,
}

fn compute_stats(times: &[f64]) -> Stats {
    let n = times.len();
    assert!(n > 0);
    let mut sorted = times.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = sorted[0];
    let max = sorted[n - 1];
    let sum: f64 = sorted.iter().sum();
    let mean = sum / n as f64;
    let median = if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    };
    let p95_idx = ((n as f64) * 0.95).ceil() as usize;
    let p95 = sorted[p95_idx.min(n - 1)];
    let variance: f64 = sorted.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / n as f64;
    let stdev = variance.sqrt();

    Stats {
        count: n,
        min_ms: min,
        max_ms: max,
        mean_ms: mean,
        median_ms: median,
        p95_ms: p95,
        stdev_ms: stdev,
    }
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

/// Python source for the "hello" workload (trivial, <1ms guest time).
const HELLO_PY: &str = "import sys, time; t0=time.time(); print(f'Hello from library bench! Python {sys.version}'); print(f'ELAPSED_GUEST_MS={int((time.time()-t0)*1000)}')";

/// Python source for the "compute" workload — jinja2 template rendering.
/// Jinja2 is pre-warmed in Hyperlight's snapshot (import=0ms) but must be
/// cold-imported on NanVix (~68ms). Not available on WSLc (stdlib only).
const COMPUTE_PY: &str = r#"import time
t0 = time.time()
from jinja2 import Template
t_import = (time.time() - t0) * 1000
tmpl = Template('<html><body><h1>{{ title }}</h1><table>{% for row in data %}<tr><td>{{ row.name }}</td><td>{{ row.value }}</td><td>{{ row.status }}</td></tr>{% endfor %}</table><p>Total: {{ count }} rows, sum={{ total }}</p></body></html>')
data = []
for i in range(10000):
    v = round((i * 17 % 997) / 10.0, 2)
    s = 'active' if i % 3 else 'inactive'
    data.append({'name': 'item_' + str(i), 'value': v, 'status': s})
total = round(sum(d['value'] for d in data), 2)
output = tmpl.render(title='Benchmark Report', data=data, count=len(data), total=total)
elapsed_ms = (time.time() - t0) * 1000
print(f'jinja2: {len(data)} rows, {len(output)} chars, import={t_import:.0f}ms, total={elapsed_ms:.0f}ms')
print(f'ELAPSED_GUEST_MS={int(elapsed_ms)}')"#;

fn workload_py_src(name: &str) -> &'static str {
    match name {
        "hello" => HELLO_PY,
        "compute" => COMPUTE_PY,
        _ => {
            eprintln!("Unknown workload '{name}'; expected: hello, compute");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

/// Build an ExecutionRequest for the given backend + workload.
fn make_request(
    backend: &ContainmentBackend,
    wslc_image: &str,
    custom_config: Option<&str>,
    workload: &str,
) -> ExecutionRequest {
    if let Some(config_path) = custom_config {
        let json = fs::read_to_string(config_path)
            .unwrap_or_else(|e| panic!("failed to read config {config_path}: {e}"));
        let mut logger = Logger::new(Mode::Buffer);
        let mut req = load_request(&json, &mut logger, false)
            .unwrap_or_else(|e| panic!("failed to parse config {config_path}: {e}"));
        req.experimental_enabled = true;
        return req;
    }

    let py_src = workload_py_src(workload);

    // Hyperlight & NanVix interpret script_code as inline Python source.
    // WSLc interprets it as a shell command line.
    let script_code = match backend {
        ContainmentBackend::Wslc => {
            // Multi-line Python scripts can't use `python3 -c "..."` with semicolons
            // (for/if blocks don't work). Use exec() with escaped newlines instead.
            let escaped = py_src.replace('\\', "\\\\").replace('\'', "\\'");
            format!("python3 -c \"exec('{}')\"", escaped.replace('\n', "\\n"))
        }
        _ => py_src.to_string(),
    };

    let mut req = ExecutionRequest {
        schema_version: "0.8.0".to_string(),
        container_id: format!("bench-lib-{}", backend.wire_name()),
        script_code,
        script_timeout: 30000,
        containment: backend.clone(),
        experimental_enabled: true,
        ..Default::default()
    };

    if matches!(backend, ContainmentBackend::Wslc) {
        req.experimental.wslc = Some(wxc_common::models::WslcConfig {
            image: wslc_image.to_string(),
            ..Default::default()
        });
    }

    req
}

// ---------------------------------------------------------------------------
// Benchmark runner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct BackendResult {
    backend: String,
    workload: String,
    warmup_iterations: usize,
    stats: Stats,
    iterations: Vec<IterationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_create_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn benchmark_backend(
    backend: &ContainmentBackend,
    warmup: usize,
    iterations: usize,
    wslc_image: &str,
    custom_config: Option<&str>,
    workload: &str,
) -> BackendResult {
    let name = backend_display_name(backend);
    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  Benchmarking: {name} (workload: {workload})");
    eprintln!("  Warmup: {warmup}, Iterations: {iterations}");
    eprintln!("{}", "=".repeat(60));

    let request = make_request(backend, wslc_image, custom_config, workload);

    // Create the runner once — this is the setup cost we want to amortize
    let t_create = Instant::now();
    let mut logger = Logger::new(Mode::Buffer);
    let resolved = match mxc_engine::resolve_runner(&request, &mut logger) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("failed to create {name} runner: {e}");
            eprintln!("  ERROR: {msg}");
            return BackendResult {
                backend: name.to_string(),
                workload: workload.to_string(),
                warmup_iterations: warmup,
                stats: Stats {
                    count: 0,
                    min_ms: 0.0,
                    max_ms: 0.0,
                    mean_ms: 0.0,
                    median_ms: 0.0,
                    p95_ms: 0.0,
                    stdev_ms: 0.0,
                },
                iterations: vec![],
                runner_create_ms: None,
                error: Some(msg),
            };
        }
    };
    let create_ms = t_create.elapsed().as_secs_f64() * 1000.0;
    eprintln!("  Runner created in {create_ms:.1}ms");

    let mut runner = resolved.runner;

    // Warmup iterations
    for w in 0..warmup {
        let t = Instant::now();
        let resp = runner.execute(&request, &mut Logger::new(Mode::Buffer));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "  [warmup {}/{}] {ms:.1}ms exit={}",
            w + 1,
            warmup,
            resp.exit_code
        );
    }

    // Timed iterations
    let mut results = Vec::with_capacity(iterations);
    let mut times = Vec::with_capacity(iterations);

    for i in 0..iterations {
        let t = Instant::now();
        let resp: ScriptResponse = runner.execute(&request, &mut Logger::new(Mode::Buffer));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        times.push(ms);

        let stdout_preview = resp
            .standard_out
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();

        results.push(IterationResult {
            iteration: i + 1,
            elapsed_ms: ms,
            exit_code: resp.exit_code,
            stdout_preview,
        });

        eprintln!(
            "  [{}/{}] {ms:.1}ms exit={}",
            i + 1,
            iterations,
            resp.exit_code
        );
    }

    let stats = compute_stats(&times);
    eprintln!(
        "  => median={:.1}ms  mean={:.1}ms  min={:.1}ms  max={:.1}ms  p95={:.1}ms",
        stats.median_ms, stats.mean_ms, stats.min_ms, stats.max_ms, stats.p95_ms
    );

    BackendResult {
        backend: name.to_string(),
        workload: workload.to_string(),
        warmup_iterations: warmup,
        stats,
        iterations: results,
        runner_create_ms: Some(create_ms),
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Density test
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct DensityEntry {
    index: usize,
    create_ms: f64,
    execute_ms: f64,
    exit_code: i32,
    /// Process commit charge after execute returns (persistent memory).
    process_commit_mb: f64,
    /// Peak process commit polled during execute (captures ephemeral VM memory).
    peak_commit_during_exec_mb: f64,
    /// Peak external daemon commit during execute (captures nanvixd subprocess).
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_daemon_commit_mb: Option<f64>,
    /// System-wide commit used after this runner's execute (GlobalMemoryStatusEx).
    system_commit_mb: f64,
    /// Container memory from cgroup (WSLc only) — actual in-VM memory used.
    #[serde(skip_serializing_if = "Option::is_none")]
    container_memory_mb: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct DensityResult {
    backend: String,
    count: usize,
    /// Whether VM/container memory is ephemeral (freed after execute).
    ephemeral: bool,
    entries: Vec<DensityEntry>,
    baseline_commit_mb: f64,
    final_commit_mb: f64,
    /// System-wide commit baseline (captures VM memory invisible to per-process APIs).
    baseline_system_commit_mb: f64,
    final_system_commit_mb: f64,
    /// Persistent per-runner overhead (commit that stays after execute).
    per_runner_persistent_mb: f64,
    /// Peak per-execution overhead (commit during execute, includes ephemeral VM).
    per_exec_peak_mb: f64,
    /// Per-runner system-wide commit delta (includes VM memory for WSLc).
    per_runner_system_mb: f64,
    /// External daemon memory growth.
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_daemon_commit_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_daemon_commit_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_runner_daemon_mb: Option<f64>,
    /// Median per-container memory measured via cgroup (WSLc only).
    #[serde(skip_serializing_if = "Option::is_none")]
    per_container_memory_mb: Option<f64>,
    /// The cost used for density estimation:
    /// - Persistent backends (Hyperlight): persistent per-runner + daemon
    /// - Ephemeral backends (NanVix): peak per-execution + daemon
    /// - WSLc: cgroup memory from inside container (actual in-VM usage)
    density_cost_mb: f64,
    fits_in_1500mb: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Polls process commit charge and external daemon commit in a background thread
/// during execute(). Returns (ScriptResponse, exec_ms, peak_process_commit_mb, peak_daemon_commit_mb).
///
/// For NanVix, nanvixd.exe is spawned as a short-lived subprocess during
/// execute() — this captures its commit charge while it's alive.
fn execute_with_peak_commit(
    runner: &mut dyn ScriptRunner,
    request: &ExecutionRequest,
    daemon_names: Vec<String>,
) -> (ScriptResponse, f64, f64, f64) {
    let running = Arc::new(AtomicBool::new(true));
    let peak_proc = Arc::new(Mutex::new(0.0f64));
    let peak_daemon = Arc::new(Mutex::new(0.0f64));

    let running_c = running.clone();
    let peak_proc_c = peak_proc.clone();
    let peak_daemon_c = peak_daemon.clone();
    let poller = std::thread::spawn(move || {
        let names: Vec<&str> = daemon_names.iter().map(|s| s.as_str()).collect();
        while running_c.load(Ordering::Relaxed) {
            let commit = process_commit_mb();
            {
                let mut p = peak_proc_c.lock().unwrap();
                if commit > *p { *p = commit; }
            }
            if !names.is_empty() {
                let dc = external_process_commit_mb(&names);
                let mut p = peak_daemon_c.lock().unwrap();
                if dc > *p { *p = dc; }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    let t = Instant::now();
    let resp = runner.execute(request, &mut Logger::new(Mode::Buffer));
    let exec_ms = t.elapsed().as_secs_f64() * 1000.0;

    running.store(false, Ordering::Relaxed);
    poller.join().unwrap();

    let peak_c = *peak_proc.lock().unwrap();
    let peak_dc = *peak_daemon.lock().unwrap();
    (resp, exec_ms, peak_c, peak_dc)
}

fn density_test(
    n: usize,
    backends: &[ContainmentBackend],
    wslc_image: &str,
    workload: &str,
) -> Vec<DensityResult> {
    let mut results = Vec::new();

    for backend in backends {
        let name = backend_display_name(backend);
        let dnames = daemon_process_names(backend);
        let has_daemon = !dnames.is_empty();
        let ephemeral = is_ephemeral_backend(backend);
        let request = make_request(backend, wslc_image, None, workload);

        eprintln!("\n{}", "=".repeat(60));
        eprintln!("  Density test: {name} × {n} runners");
        if ephemeral {
            eprintln!("  Memory model: ephemeral (VM freed after each execute)");
        } else {
            eprintln!("  Memory model: persistent (snapshot stays in process)");
        }
        if has_daemon {
            eprintln!("  Daemon processes: {:?}", dnames);
        }
        eprintln!("{}", "=".repeat(60));

        let baseline_commit = process_commit_mb();
        let baseline_system_commit = system_commit_used_mb();
        let baseline_daemon_commit = if has_daemon {
            let dc = external_process_commit_mb(&dnames);
            eprintln!("  Baseline daemon commit: {dc:.1} MB");
            Some(dc)
        } else {
            None
        };
        eprintln!("  Baseline process commit: {baseline_commit:.1} MB");
        eprintln!("  Baseline system commit:  {baseline_system_commit:.0} MB");

        // For WSLc: create a density-specific request that appends cgroup
        // memory measurement to the command so we capture actual in-VM
        // container memory in the same execution.
        let is_wslc = matches!(backend, ContainmentBackend::Wslc);
        let density_request = if is_wslc {
            let mut req = make_request(backend, wslc_image, None, workload);
            req.script_code = format!(
                "{} ; echo __CGROUP_MEM__=$(cat /sys/fs/cgroup/memory.current 2>/dev/null || cat /sys/fs/cgroup/memory/memory.usage_in_bytes 2>/dev/null || echo 0)",
                req.script_code
            );
            req
        } else {
            request
        };

        let mut runners = Vec::with_capacity(n);
        let mut entries = Vec::with_capacity(n);

        for i in 0..n {
            // Create runner
            let t_create = Instant::now();
            let mut logger = Logger::new(Mode::Buffer);
            let resolved = match mxc_engine::resolve_runner(&density_request, &mut logger) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("  [{name}] runner {}/{n} FAILED: {e}", i + 1);
                    break;
                }
            };
            let create_ms = t_create.elapsed().as_secs_f64() * 1000.0;
            runners.push(resolved);

            // Execute once with peak commit polling (also polls daemon commit during execute)
            let daemon_names_owned: Vec<String> = dnames.iter().map(|s| s.to_string()).collect();
            let (resp, exec_ms, peak_commit, peak_dc) = execute_with_peak_commit(
                runners.last_mut().unwrap().runner.as_mut(),
                &density_request,
                daemon_names_owned,
            );

            let commit = process_commit_mb();
            let sys_commit = system_commit_used_mb();

            // Parse cgroup memory from stdout for WSLc
            let container_mem = if is_wslc {
                resp.standard_out.lines()
                    .find_map(|line| {
                        line.strip_prefix("__CGROUP_MEM__=")
                            .and_then(|v| v.trim().parse::<f64>().ok())
                            .map(|bytes| bytes / (1024.0 * 1024.0))
                    })
            } else {
                None
            };

            eprintln!(
                "  [{name}] runner {}/{n}: create={create_ms:.1}ms  exec={exec_ms:.1}ms  exit={}  commit={commit:.1}MB  peakCommit={peak_commit:.1}MB{}{}",
                i + 1,
                resp.exit_code,
                if has_daemon { format!("  peakDaemon={peak_dc:.1}MB") } else { String::new() },
                if let Some(cm) = container_mem { format!("  containerMem={cm:.1}MB") } else { String::new() },
            );

            entries.push(DensityEntry {
                index: i + 1,
                create_ms,
                execute_ms: exec_ms,
                exit_code: resp.exit_code,
                process_commit_mb: commit,
                peak_commit_during_exec_mb: peak_commit,
                peak_daemon_commit_mb: if has_daemon { Some(peak_dc) } else { None },
                system_commit_mb: sys_commit,
                container_memory_mb: container_mem,
            });
        }

        let final_commit = process_commit_mb();
        let final_system_commit = system_commit_used_mb();
        let final_daemon_commit = if has_daemon {
            Some(external_process_commit_mb(&dnames))
        } else {
            None
        };

        let alive = runners.len();
        let per_runner_persistent = if alive > 0 {
            (final_commit - baseline_commit) / alive as f64
        } else {
            0.0
        };

        // System-wide commit delta per runner — captures VM memory (vmmem) that
        // per-process APIs can't see. Used for WSLc density estimation.
        let per_runner_system = if alive > 0 {
            (final_system_commit - baseline_system_commit) / alive as f64
        } else {
            0.0
        };

        // For ephemeral backends, peak-during-execute captures the VM cost.
        // We take the median peak delta across all executions.
        let per_exec_peak = if !entries.is_empty() {
            let mut deltas: Vec<f64> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let before = if i == 0 {
                        baseline_commit
                    } else {
                        entries[i - 1].process_commit_mb
                    };
                    (e.peak_commit_during_exec_mb - before).max(0.0)
                })
                .collect();
            deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
            deltas[deltas.len() / 2] // median
        } else {
            0.0
        };

        // Daemon cost depends on whether it's a subprocess or persistent service:
        // - nanvixd: short-lived subprocess (baseline commit = 0). Per-exec cost = peak commit.
        // - wslservice: persistent service (baseline > 0). Per-exec cost = peak - baseline.
        let per_runner_daemon = if has_daemon && !entries.is_empty() {
            let baseline_dc = baseline_daemon_commit.unwrap_or(0.0);
            let mut deltas: Vec<f64> = entries
                .iter()
                .filter_map(|e| e.peak_daemon_commit_mb.map(|p| (p - baseline_dc).max(0.0)))
                .collect();
            deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if !deltas.is_empty() {
                Some(deltas[deltas.len() / 2]) // median peak delta
            } else {
                None
            }
        } else {
            None
        };

        // Median container memory from cgroup (WSLc only)
        let per_container_memory = if is_wslc {
            let mut mems: Vec<f64> = entries.iter()
                .filter_map(|e| e.container_memory_mb)
                .collect();
            mems.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if !mems.is_empty() { Some(mems[mems.len() / 2]) } else { None }
        } else {
            None
        };

        // Each backend has a different memory architecture, so we measure
        // where the VM/container memory actually lives:
        //
        // - Hyperlight (persistent, in-process): VM snapshot loaded into the
        //   bench-library process via WHP WHvMapGpaRange. The per-process
        //   commit growth captures the user-space mapping cost per runner.
        //
        // - NanVix (ephemeral, subprocess): each execute() spawns nanvixd.exe
        //   which commits the full VM address space (~258 MB). The subprocess
        //   is short-lived — memory is freed after each call. We capture its
        //   peak commit by polling during execution.
        //
        // - WSLc (ephemeral, VM-hosted): each execute() creates a container
        //   inside the shared WSL2 VM. Container memory is invisible to host
        //   process APIs (it lives in the vmmem kernel pseudo-process). We
        //   measure from inside the container via Linux cgroup stats
        //   (/sys/fs/cgroup/memory.current).
        let density_cost = if is_wslc {
            per_container_memory.unwrap_or(per_runner_system.max(0.0))
        } else if ephemeral {
            per_exec_peak + per_runner_daemon.unwrap_or(0.0)
        } else {
            per_runner_persistent + per_runner_daemon.unwrap_or(0.0)
        };

        let fits = if density_cost > 0.0 {
            ((1500.0 - baseline_commit) / density_cost).floor() as usize
        } else {
            0
        };

        eprintln!("  [{name}] {alive}/{n} runners alive");
        eprintln!(
            "  [{name}] Persistent per-runner: {per_runner_persistent:.1} MB"
        );
        eprintln!(
            "  [{name}] Peak per-execution:    {per_exec_peak:.1} MB"
        );
        eprintln!(
            "  [{name}] System commit delta:   {per_runner_system:.1} MB/runner"
        );
        if let Some(cm) = per_container_memory {
            eprintln!("  [{name}] Container memory (cgroup): {cm:.1} MB/container");
        }
        if let (Some(d), Some(b)) = (per_runner_daemon, baseline_daemon_commit) {
            let label = if b < 0.1 { "subprocess" } else { "service delta" };
            eprintln!(
                "  [{name}] Daemon ({label}):      {d:.1} MB/exec  (baseline={b:.1}MB)"
            );
        }
        eprintln!(
            "  [{name}] => Density cost: {density_cost:.1} MB/runner  (~{fits} fit in 1.5 GB)"
        );

        let note: Option<String> = None;

        results.push(DensityResult {
            backend: name.to_string(),
            count: alive,
            ephemeral,
            entries,
            baseline_commit_mb: baseline_commit,
            final_commit_mb: final_commit,
            baseline_system_commit_mb: baseline_system_commit,
            final_system_commit_mb: final_system_commit,
            per_runner_persistent_mb: per_runner_persistent,
            per_exec_peak_mb: per_exec_peak,
            per_runner_system_mb: per_runner_system,
            daemon_names: if has_daemon {
                Some(dnames.iter().map(|s| s.to_string()).collect())
            } else {
                None
            },
            baseline_daemon_commit_mb: baseline_daemon_commit,
            final_daemon_commit_mb: final_daemon_commit,
            per_runner_daemon_mb: per_runner_daemon,
            per_container_memory_mb: per_container_memory,
            density_cost_mb: density_cost,
            fits_in_1500mb: fits,
            note,
        });

        // Drop runners before next backend
        drop(runners);
    }

    results
}

// ---------------------------------------------------------------------------
// HTML report generation
// ---------------------------------------------------------------------------

fn generate_html(results: &[BackendResult]) -> String {
    let json_data = serde_json::to_string_pretty(results).unwrap_or_default();

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>MXC Library-Mode Benchmark</title>
<style>
  :root {{
    --bg: #fff; --fg: #1a1a2e; --card-bg: #f8f9fa; --border: #dee2e6;
    --accent1: #2563eb; --accent2: #dc2626; --accent3: #059669;
    --grid: #e9ecef; --muted: #6c757d;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --bg: #0d1117; --fg: #e6edf3; --card-bg: #161b22; --border: #30363d;
      --accent1: #58a6ff; --accent2: #f85149; --accent3: #3fb950;
      --grid: #21262d; --muted: #8b949e;
    }}
  }}
  :root[data-theme="dark"] {{
    --bg: #0d1117; --fg: #e6edf3; --card-bg: #161b22; --border: #30363d;
    --accent1: #58a6ff; --accent2: #f85149; --accent3: #3fb950;
    --grid: #21262d; --muted: #8b949e;
  }}
  :root[data-theme="light"] {{
    --bg: #fff; --fg: #1a1a2e; --card-bg: #f8f9fa; --border: #dee2e6;
    --accent1: #2563eb; --accent2: #dc2626; --accent3: #059669;
    --grid: #e9ecef; --muted: #6c757d;
  }}

  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: var(--bg); color: var(--fg);
    max-width: 1100px; margin: 0 auto; padding: 2rem 1rem;
  }}
  h1 {{ font-size: 1.6rem; margin-bottom: 0.25rem; }}
  .subtitle {{ color: var(--muted); margin-bottom: 2rem; font-size: 0.9rem; }}
  .stats-grid {{
    display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
    gap: 1rem; margin-bottom: 2rem;
  }}
  .stat-card {{
    background: var(--card-bg); border: 1px solid var(--border);
    border-radius: 8px; padding: 1rem;
  }}
  .stat-card .label {{ font-size: 0.75rem; color: var(--muted); text-transform: uppercase; letter-spacing: 0.05em; }}
  .stat-card .value {{ font-size: 1.5rem; font-weight: 700; font-variant-numeric: tabular-nums; }}
  .stat-card .detail {{ font-size: 0.8rem; color: var(--muted); margin-top: 0.25rem; }}

  .chart-container {{ background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; }}
  .chart-title {{ font-size: 1rem; font-weight: 600; margin-bottom: 1rem; }}
  canvas {{ width: 100% !important; }}

  table {{
    width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums;
    font-size: 0.85rem; margin-top: 1rem;
  }}
  th, td {{ padding: 0.5rem 0.75rem; text-align: right; border-bottom: 1px solid var(--border); }}
  th {{ text-align: right; font-weight: 600; font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted); }}
  th:first-child, td:first-child {{ text-align: left; }}
  .error-row {{ color: var(--accent2); }}
</style>
</head>
<body>

<h1>MXC Library-Mode Benchmark</h1>
<p class="subtitle">In-process runner reuse — steady-state latency per backend</p>

<div class="stats-grid" id="statsGrid"></div>
<div class="chart-container">
  <div class="chart-title">Latency per Iteration (ms)</div>
  <canvas id="stripChart" height="300"></canvas>
</div>
<div class="chart-container">
  <div class="chart-title">Detailed Statistics</div>
  <table id="statsTable"></table>
</div>

<script>
const DATA = {json_data};
const COLORS = ['#2563eb', '#dc2626', '#059669', '#f59e0b'];

// Stats cards
const grid = document.getElementById('statsGrid');
DATA.filter(d => !d.error).forEach((d, i) => {{
  const card = document.createElement('div');
  card.className = 'stat-card';
  card.innerHTML = `
    <div class="label">${{d.backend}}</div>
    <div class="value" style="color:${{COLORS[i]}}">${{d.stats.median_ms.toFixed(1)}}ms</div>
    <div class="detail">median · ${{d.stats.count}} runs · setup ${{(d.runner_create_ms||0).toFixed(0)}}ms</div>
  `;
  grid.appendChild(card);
}});

// Strip chart
const canvas = document.getElementById('stripChart');
const ctx = canvas.getContext('2d');
function drawChart() {{
  const dpr = window.devicePixelRatio || 1;
  const rect = canvas.getBoundingClientRect();
  canvas.width = rect.width * dpr;
  canvas.height = rect.height * dpr;
  ctx.scale(dpr, dpr);
  const W = rect.width, H = rect.height;
  const pad = {{ top: 20, right: 20, bottom: 40, left: 60 }};
  const plotW = W - pad.left - pad.right;
  const plotH = H - pad.top - pad.bottom;

  const valid = DATA.filter(d => !d.error);
  if (!valid.length) return;
  const maxIter = Math.max(...valid.map(d => d.iterations.length));
  const allTimes = valid.flatMap(d => d.iterations.map(it => it.elapsed_ms));
  const yMax = Math.max(...allTimes) * 1.1;

  const cs = getComputedStyle(document.documentElement);
  const fg = cs.getPropertyValue('--fg').trim() || '#333';
  const gridC = cs.getPropertyValue('--grid').trim() || '#eee';

  // Y grid
  ctx.strokeStyle = gridC; ctx.lineWidth = 0.5;
  const yTicks = 5;
  for (let i = 0; i <= yTicks; i++) {{
    const y = pad.top + plotH - (i / yTicks) * plotH;
    ctx.beginPath(); ctx.moveTo(pad.left, y); ctx.lineTo(pad.left + plotW, y); ctx.stroke();
    ctx.fillStyle = fg; ctx.font = '11px sans-serif'; ctx.textAlign = 'right';
    ctx.fillText((yMax * i / yTicks).toFixed(0), pad.left - 8, y + 4);
  }}

  // X axis label
  ctx.fillStyle = fg; ctx.font = '11px sans-serif'; ctx.textAlign = 'center';
  ctx.fillText('Iteration', pad.left + plotW / 2, H - 5);

  // Plot each backend
  valid.forEach((d, di) => {{
    const color = COLORS[di % COLORS.length];
    ctx.strokeStyle = color; ctx.lineWidth = 2;
    ctx.beginPath();
    d.iterations.forEach((it, j) => {{
      const x = pad.left + (j / (maxIter - 1 || 1)) * plotW;
      const y = pad.top + plotH - (it.elapsed_ms / yMax) * plotH;
      if (j === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }});
    ctx.stroke();

    // Dots
    ctx.fillStyle = color;
    d.iterations.forEach((it, j) => {{
      const x = pad.left + (j / (maxIter - 1 || 1)) * plotW;
      const y = pad.top + plotH - (it.elapsed_ms / yMax) * plotH;
      ctx.beginPath(); ctx.arc(x, y, 3, 0, Math.PI * 2); ctx.fill();
    }});

    // Legend
    const lx = pad.left + 10 + di * 160;
    const ly = pad.top + 14;
    ctx.fillStyle = color;
    ctx.fillRect(lx, ly - 8, 12, 12);
    ctx.fillStyle = fg; ctx.textAlign = 'left'; ctx.font = '12px sans-serif';
    ctx.fillText(d.backend, lx + 16, ly + 2);
  }});
}}
drawChart();
window.addEventListener('resize', drawChart);

// Stats table
const table = document.getElementById('statsTable');
let html = '<thead><tr><th>Backend</th><th>Median</th><th>Mean</th><th>Min</th><th>Max</th><th>P95</th><th>Stdev</th><th>Setup</th></tr></thead><tbody>';
DATA.forEach(d => {{
  if (d.error) {{
    html += `<tr class="error-row"><td>${{d.backend}}</td><td colspan="7">${{d.error}}</td></tr>`;
  }} else {{
    const s = d.stats;
    html += `<tr><td>${{d.backend}}</td><td>${{s.median_ms.toFixed(1)}}</td><td>${{s.mean_ms.toFixed(1)}}</td><td>${{s.min_ms.toFixed(1)}}</td><td>${{s.max_ms.toFixed(1)}}</td><td>${{s.p95_ms.toFixed(1)}}</td><td>${{s.stdev_ms.toFixed(1)}}</td><td>${{(d.runner_create_ms||0).toFixed(0)}}ms</td></tr>`;
  }}
}});
html += '</tbody>';
table.innerHTML = html;
</script>
</body>
</html>"##
    )
}

// ---------------------------------------------------------------------------
// Cold-start benchmark (spawn wxc-exec as subprocess)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ColdStartEntry {
    iteration: usize,
    elapsed_ms: f64,
    exit_code: i32,
    peak_commit_mb: f64,
}

#[derive(Debug, Clone, Serialize)]
struct ColdStartResult {
    backend: String,
    stats: Stats,
    entries: Vec<ColdStartEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Escape a string for embedding in JSON (handles newlines, quotes, backslashes).
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Write a temp wxc-exec config for the given backend + workload.
fn write_temp_config(
    backend: &ContainmentBackend,
    wslc_image: &str,
    workload: &str,
) -> std::path::PathBuf {
    let py_src = workload_py_src(workload);
    let escaped = json_escape(py_src);
    let json = match backend {
        ContainmentBackend::Hyperlight => format!(
            r#"{{"process":{{"commandLine":"{escaped}","timeout":30000}},"containment":"hyperlight"}}"#,
        ),
        ContainmentBackend::MicroVm => format!(
            r#"{{"process":{{"commandLine":"{escaped}","timeout":30000}},"containment":"microvm"}}"#,
        ),
        ContainmentBackend::Wslc => {
            let cmd = format!("python3 -c \\\"{}\\\"", py_src.replace('\n', "; ").replace('"', "\\\""));
            format!(
                r#"{{"version":"0.8.0","containerId":"bench-cold-wslc","containment":"wslc","process":{{"commandLine":"{cmd}","timeout":30000}},"network":{{"defaultPolicy":"block"}},"experimental":{{"wslc":{{"image":"{wslc_image}"}}}}}}"#
            )
        }
        _ => String::new(),
    };
    let dir = std::env::temp_dir().join("mxc-bench");
    fs::create_dir_all(&dir).ok();
    let name = backend_display_name(backend).replace(' ', "_").replace('(', "").replace(')', "").to_lowercase();
    let path = dir.join(format!("cold_{name}.json"));
    fs::write(&path, &json).expect("write temp config");
    path
}

fn cold_start_benchmark(
    backend: &ContainmentBackend,
    warmup: usize,
    iterations: usize,
    wslc_image: &str,
    workload: &str,
) -> ColdStartResult {
    let name = backend_display_name(backend);
    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  Cold-start: {name}");
    eprintln!("  Warmup: {warmup}, Iterations: {iterations}");
    eprintln!("{}", "=".repeat(60));

    // Find wxc-exec next to bench-library
    let exe_dir = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("exe parent")
        .to_path_buf();
    let wxc_exec = exe_dir.join("wxc-exec.exe");
    if !wxc_exec.exists() {
        let msg = format!("wxc-exec.exe not found at {}", wxc_exec.display());
        eprintln!("  ERROR: {msg}");
        return ColdStartResult {
            backend: name.to_string(),
            stats: Stats { count: 0, min_ms: 0.0, max_ms: 0.0, mean_ms: 0.0, median_ms: 0.0, p95_ms: 0.0, stdev_ms: 0.0 },
            entries: vec![],
            error: Some(msg),
        };
    }

    let config_path = write_temp_config(backend, wslc_image, workload);

    // Helper: run one cold-start iteration
    let run_one = |_iter: usize| -> Option<(f64, i32, f64)> {
        use std::process::{Command, Stdio};

        let t = Instant::now();
        let mut child = Command::new(&wxc_exec)
            .arg(config_path.to_str().unwrap())
            .args(&["--experimental", "--debug"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let status = child.wait().ok()?;
        let elapsed_ms = t.elapsed().as_secs_f64() * 1000.0;
        let exit_code = status.code().unwrap_or(-1);

        #[cfg(target_os = "windows")]
        let peak_commit = {
            use std::os::windows::io::AsRawHandle;
            process_peak_commit_by_handle(child.as_raw_handle() as isize)
        };
        #[cfg(not(target_os = "windows"))]
        let peak_commit = 0.0;

        Some((elapsed_ms, exit_code, peak_commit))
    };

    // Warmup
    for w in 0..warmup {
        if let Some((ms, exit, _)) = run_one(w) {
            eprintln!("  [warmup {}/{}] {ms:.1}ms exit={exit}", w + 1, warmup);
        }
    }

    // Timed iterations
    let mut entries = Vec::with_capacity(iterations);
    let mut times = Vec::with_capacity(iterations);

    for i in 0..iterations {
        match run_one(i) {
            Some((ms, exit, peak_commit)) => {
                times.push(ms);
                entries.push(ColdStartEntry {
                    iteration: i + 1,
                    elapsed_ms: ms,
                    exit_code: exit,
                    peak_commit_mb: peak_commit,
                });
                eprintln!("  [{}/{}] {ms:.1}ms exit={exit} peakCommit={peak_commit:.1}MB", i + 1, iterations);
            }
            None => {
                eprintln!("  [{}/{}] FAILED to spawn", i + 1, iterations);
            }
        }
    }

    if times.is_empty() {
        return ColdStartResult {
            backend: name.to_string(),
            stats: Stats { count: 0, min_ms: 0.0, max_ms: 0.0, mean_ms: 0.0, median_ms: 0.0, p95_ms: 0.0, stdev_ms: 0.0 },
            entries,
            error: Some("all iterations failed".into()),
        };
    }

    let stats = compute_stats(&times);
    eprintln!(
        "  => median={:.1}ms  p99={:.1}ms  peakCommit={:.1}MB",
        stats.median_ms, stats.p95_ms,
        entries.iter().map(|e| e.peak_commit_mb).fold(0.0f64, f64::max)
    );

    ColdStartResult { backend: name.to_string(), stats, entries, error: None }
}

// ---------------------------------------------------------------------------
// Disk measurement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct DiskResult {
    backend: String,
    total_mb: f64,
    files: Vec<DiskFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DiskFile {
    name: String,
    logical_mb: f64,
    actual_mb: f64,
}

/// Measure a single file's disk usage (sparse-aware on Windows).
fn measure_file(path: &std::path::Path) -> Option<DiskFile> {
    if !path.exists() {
        return None;
    }
    let logical = path.metadata().map(|m| m.len() as f64 / (1024.0 * 1024.0)).unwrap_or(0.0);
    #[cfg(target_os = "windows")]
    let actual = file_actual_size_mb(path);
    #[cfg(not(target_os = "windows"))]
    let actual = logical;
    Some(DiskFile {
        name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
        logical_mb: logical,
        actual_mb: actual,
    })
}

/// Recursively measure all files in a directory (sparse-aware).
fn measure_dir_recursive(dir: &std::path::Path) -> Vec<DiskFile> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return files;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(measure_dir_recursive(&path));
            } else if let Some(f) = measure_file(&path) {
                files.push(f);
            }
        }
    }
    files
}

fn measure_disk(backend: &ContainmentBackend, wslc_image: &str) -> DiskResult {
    let name = backend_display_name(backend);
    let exe_dir = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("exe parent")
        .to_path_buf();

    match backend {
        ContainmentBackend::Hyperlight => {
            // Hyperlight's snapshot lives in %LOCALAPPDATA%\pyhl\snapshot\ (installed
            // by `wxc-exec --setup-hyperlight`). The snapshot blob is NTFS sparse —
            // we measure actual on-disk allocation. The initrd.cpio is baked into the
            // snapshot, so only the snapshot directory counts.
            let snapshot_dir = resolve_localappdata().join("pyhl").join("snapshot");
            if snapshot_dir.is_dir() {
                let files = measure_dir_recursive(&snapshot_dir);
                let total: f64 = files.iter().map(|f| f.actual_mb).sum();
                eprintln!("  [Hyperlight] disk: {total:.1} MB actual in {}", snapshot_dir.display());
                for f in &files {
                    if f.actual_mb > 1.0 {
                        eprintln!("    {} — logical={:.1} MB, on-disk={:.1} MB", f.name, f.logical_mb, f.actual_mb);
                    }
                }
                DiskResult { backend: name.to_string(), total_mb: total, files, note: None }
            } else {
                eprintln!("  [Hyperlight] disk: snapshot not found at {}", snapshot_dir.display());
                DiskResult {
                    backend: name.to_string(), total_mb: 0.0, files: vec![],
                    note: Some(format!("snapshot not found at {}", snapshot_dir.display())),
                }
            }
        }
        ContainmentBackend::MicroVm => {
            // NanVix runtime files: nanvixd binary, rootfs image, initrd, and kernel ELF.
            // The WHP snapshot (snapshots/kernel.vmem) is auto-generated on first boot
            // and shared with Hyperlight, so it's not counted here.
            let file_list = vec![
                exe_dir.join("nanvixd.exe"),
                exe_dir.join("nanvix_rootfs.img"),
                exe_dir.join("python3.initrd"),
                exe_dir.join("bin").join("kernel.elf"),
            ];
            let files: Vec<DiskFile> = file_list.iter().filter_map(|p| measure_file(p)).collect();
            let total: f64 = files.iter().map(|f| f.actual_mb).sum();
            eprintln!("  [NanVix] disk: {total:.1} MB");
            for f in &files {
                eprintln!("    {} — {:.1} MB", f.name, f.actual_mb);
            }
            DiskResult { backend: name.to_string(), total_mb: total, files, note: None }
        }
        ContainmentBackend::Wslc => {
            // WSLc needs:
            //   1. The WSL2 runtime: system.vhd (kernel+OS image, equivalent to
            //      Hyperlight's VM snapshot), wslservice.exe, container.exe, etc.
            //   2. The OCI container image (rootfs measured from inside via du).
            eprintln!("  [WSLc] disk: measuring WSL2 runtime + OCI image...");

            let wsl_dir = std::path::PathBuf::from(r"C:\Program Files\WSL");
            // Core WSLc runtime files (excludes GPU passthrough, RDP, and WSLg
            // which are optional features not needed for container execution).
            let runtime_files = vec![
                wsl_dir.join("system.vhd"),        // WSL2 kernel+OS image
                wsl_dir.join("wslservice.exe"),     // Container service
                wsl_dir.join("container.exe"),      // Container runtime
                wsl_dir.join("wslc.exe"),           // WSLc CLI
                wsl_dir.join("wslcsession.exe"),    // Session manager
                wsl_dir.join("wsl.exe"),            // WSL CLI
                wsl_dir.join("wslhost.exe"),        // Host process
                wsl_dir.join("libwsl.dll"),         // WSL library
                wsl_dir.join("wslrelay.exe"),       // Relay
                wsl_dir.join("wsldeps.dll"),        // Dependencies
                wsl_dir.join("wsldevicehost.dll"),  // Device host
            ];
            let mut files: Vec<DiskFile> = runtime_files
                .iter()
                .filter_map(|p| measure_file(p))
                .collect();
            for f in &files {
                eprintln!("    {} — {:.1} MB", f.name, f.actual_mb);
            }

            // OCI image rootfs (measured from inside a temporary container)
            let mut req = ExecutionRequest {
                schema_version: "0.8.0".to_string(),
                container_id: "bench-disk-wslc".to_string(),
                script_code: "du -sx / 2>/dev/null | head -1".to_string(),
                script_timeout: 30000,
                containment: ContainmentBackend::Wslc,
                experimental_enabled: true,
                ..Default::default()
            };
            req.experimental.wslc = Some(wxc_common::models::WslcConfig {
                image: wslc_image.to_string(),
                ..Default::default()
            });

            let mut logger = Logger::new(Mode::Buffer);
            if let Ok(resolved) = mxc_engine::resolve_runner(&req, &mut logger) {
                let mut runner = resolved.runner;
                let resp = runner.execute(&req, &mut Logger::new(Mode::Buffer));
                if let Some(image_mb) = resp.standard_out.lines().next().and_then(|line| {
                    let kb: f64 = line.split_whitespace().next()?.parse().ok()?;
                    Some(kb / 1024.0)
                }) {
                    eprintln!("    {} (rootfs) — {:.1} MB", wslc_image, image_mb);
                    files.push(DiskFile {
                        name: format!("{wslc_image} (rootfs)"),
                        logical_mb: image_mb,
                        actual_mb: image_mb,
                    });
                }
            }

            let total: f64 = files.iter().map(|f| f.actual_mb).sum();
            eprintln!("  [WSLc] disk: {total:.1} MB total");
            DiskResult { backend: name.to_string(), total_mb: total, files, note: None }
        }
        _ => DiskResult { backend: name.to_string(), total_mb: 0.0, files: vec![], note: None },
    }
}

/// Resolve %LOCALAPPDATA% on Windows, fall back to $HOME/AppData/Local.
fn resolve_localappdata() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(v) = std::env::var_os("LOCALAPPDATA") {
            return std::path::PathBuf::from(v);
        }
    }
    // Fallback
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        return std::path::PathBuf::from(home).join("AppData").join("Local");
    }
    std::path::PathBuf::from(".")
}

// ---------------------------------------------------------------------------
// Parallel benchmark
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ParallelEntry {
    runner_index: usize,
    elapsed_ms: f64,
    exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
struct ParallelRound {
    round: usize,
    wall_clock_ms: f64,
    entries: Vec<ParallelEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct ParallelResult {
    backend: String,
    concurrency: usize,
    rounds: Vec<ParallelRound>,
    /// Stats across per-round wall-clock times (max latency per round).
    wall_clock_stats: Stats,
    /// Stats across all individual runner latencies (shows contention impact).
    per_runner_stats: Stats,
    /// Throughput: concurrency / median_wall_clock_sec.
    throughput_per_sec: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn parallel_benchmark(
    backend: &ContainmentBackend,
    concurrency: usize,
    warmup: usize,
    iterations: usize,
    wslc_image: &str,
    workload: &str,
) -> ParallelResult {
    let name = backend_display_name(backend);
    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  Parallel: {name} × {concurrency} concurrent");
    eprintln!("  Warmup: {warmup}, Rounds: {iterations}");
    eprintln!("{}", "=".repeat(60));

    // Each thread creates its own runner, warms up, then synchronizes via barrier.
    // The barrier has concurrency+1 participants: N workers + main thread (for timing).
    let barrier = Arc::new(std::sync::Barrier::new(concurrency + 1));
    let round_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_rounds = warmup + iterations;
    let all_entries: Arc<Mutex<Vec<(usize, usize, f64, i32)>>> =
        Arc::new(Mutex::new(Vec::new()));

    let wslc_image_owned = wslc_image.to_string();
    let workload_owned = workload.to_string();
    let backend_clone = backend.clone();

    let handles: Vec<_> = (0..concurrency)
        .map(|idx| {
            let barrier = barrier.clone();
            let round_counter = round_counter.clone();
            let all_entries = all_entries.clone();
            let wslc_img = wslc_image_owned.clone();
            let wl = workload_owned.clone();
            let be = backend_clone.clone();

            std::thread::spawn(move || {
                let request = make_request(&be, &wslc_img, None, &wl);
                let mut logger = Logger::new(Mode::Buffer);
                let resolved = match mxc_engine::resolve_runner(&request, &mut logger) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("  [thread {idx}] failed to create runner: {e}");
                        // Still participate in barriers so other threads don't deadlock
                        for _ in 0..total_rounds {
                            barrier.wait();
                            barrier.wait();
                        }
                        return;
                    }
                };
                let mut runner = resolved.runner;

                // Warmup (each thread warms up its own runner independently)
                for _ in 0..warmup {
                    runner.execute(&request, &mut Logger::new(Mode::Buffer));
                }

                for _ in 0..total_rounds {
                    // Wait for main thread to signal round start
                    barrier.wait();

                    let round = round_counter.load(Ordering::Relaxed);

                    let t = Instant::now();
                    let resp = runner.execute(&request, &mut Logger::new(Mode::Buffer));
                    let ms = t.elapsed().as_secs_f64() * 1000.0;

                    {
                        let mut entries = all_entries.lock().unwrap();
                        entries.push((round, idx, ms, resp.exit_code));
                    }

                    // Wait for all threads to finish this round
                    barrier.wait();
                }
            })
        })
        .collect();

    // Main thread drives the rounds
    let mut rounds = Vec::with_capacity(iterations);
    let mut wall_clocks = Vec::new();
    let mut all_latencies = Vec::new();

    for round_idx in 0..total_rounds {
        round_counter.store(round_idx, Ordering::Relaxed);

        // Signal round start
        barrier.wait();

        let wall_t = Instant::now();

        // Wait for all threads to finish
        barrier.wait();

        let wall_ms = wall_t.elapsed().as_secs_f64() * 1000.0;

        // Collect results for this round
        let entries_lock = all_entries.lock().unwrap();
        let round_entries: Vec<ParallelEntry> = entries_lock
            .iter()
            .filter(|(r, _, _, _)| *r == round_idx)
            .map(|(_, idx, ms, exit)| ParallelEntry {
                runner_index: *idx,
                elapsed_ms: *ms,
                exit_code: *exit,
            })
            .collect();
        drop(entries_lock);

        let is_warmup = round_idx < warmup;
        let label = if is_warmup { "warmup" } else { "timed" };
        eprintln!(
            "  [round {}/{} {label}] wall={wall_ms:.1}ms  runners={}  latencies={:.1}-{:.1}ms",
            round_idx + 1,
            total_rounds,
            round_entries.len(),
            round_entries.iter().map(|e| e.elapsed_ms).fold(f64::MAX, f64::min),
            round_entries.iter().map(|e| e.elapsed_ms).fold(0.0f64, f64::max),
        );

        if !is_warmup {
            wall_clocks.push(wall_ms);
            for e in &round_entries {
                all_latencies.push(e.elapsed_ms);
            }
            rounds.push(ParallelRound {
                round: round_idx - warmup + 1,
                wall_clock_ms: wall_ms,
                entries: round_entries,
            });
        }
    }

    // Join all threads
    for h in handles {
        h.join().ok();
    }

    if wall_clocks.is_empty() || all_latencies.is_empty() {
        return ParallelResult {
            backend: name.to_string(),
            concurrency,
            rounds: vec![],
            wall_clock_stats: Stats { count: 0, min_ms: 0.0, max_ms: 0.0, mean_ms: 0.0, median_ms: 0.0, p95_ms: 0.0, stdev_ms: 0.0 },
            per_runner_stats: Stats { count: 0, min_ms: 0.0, max_ms: 0.0, mean_ms: 0.0, median_ms: 0.0, p95_ms: 0.0, stdev_ms: 0.0 },
            throughput_per_sec: 0.0,
            error: Some("no successful rounds".into()),
        };
    }

    let wall_stats = compute_stats(&wall_clocks);
    let runner_stats = compute_stats(&all_latencies);
    let throughput = if wall_stats.median_ms > 0.0 {
        concurrency as f64 / (wall_stats.median_ms / 1000.0)
    } else {
        0.0
    };

    eprintln!(
        "  => wall-clock: median={:.1}ms  throughput={:.0} exec/sec",
        wall_stats.median_ms, throughput
    );
    eprintln!(
        "  => per-runner: median={:.1}ms  mean={:.1}ms  p95={:.1}ms",
        runner_stats.median_ms, runner_stats.mean_ms, runner_stats.p95_ms
    );

    ParallelResult {
        backend: name.to_string(),
        concurrency,
        rounds,
        wall_clock_stats: wall_stats,
        per_runner_stats: runner_stats,
        throughput_per_sec: throughput,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Full benchmark (one command → one HTML)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct WorkloadResult {
    workload: String,
    warm_start: Vec<BackendResult>,
    cold_start: Vec<ColdStartResult>,
    density: Vec<DensityResult>,
    parallel: Vec<ParallelResult>,
}

#[derive(Debug, Clone, Serialize)]
struct FullResult {
    workloads: Vec<WorkloadResult>,
    disk: Vec<DiskResult>,  // workload-independent
}

fn generate_full_html(result: &FullResult) -> String {
    let json_data = serde_json::to_string(&result).unwrap_or_default();

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>MXC Containment Benchmark</title>
<style>
  :root {{
    --bg: #fafafa; --fg: #1a1a2e; --card-bg: #fff; --border: #e0e0e0;
    --accent1: #2563eb; --accent2: #dc2626; --accent3: #059669;
    --grid: #f0f0f0; --muted: #666; --header-bg: #f5f5f5;
    --tab-active: #2563eb; --tab-inactive: transparent;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --bg: #0d1117; --fg: #e6edf3; --card-bg: #161b22; --border: #30363d;
      --accent1: #58a6ff; --accent2: #f85149; --accent3: #3fb950;
      --grid: #21262d; --muted: #8b949e; --header-bg: #1c2128;
      --tab-active: #58a6ff;
    }}
  }}
  :root[data-theme="dark"] {{
    --bg: #0d1117; --fg: #e6edf3; --card-bg: #161b22; --border: #30363d;
    --accent1: #58a6ff; --accent2: #f85149; --accent3: #3fb950;
    --grid: #21262d; --muted: #8b949e; --header-bg: #1c2128;
    --tab-active: #58a6ff;
  }}
  :root[data-theme="light"] {{
    --bg: #fafafa; --fg: #1a1a2e; --card-bg: #fff; --border: #e0e0e0;
    --accent1: #2563eb; --accent2: #dc2626; --accent3: #059669;
    --grid: #f0f0f0; --muted: #666; --header-bg: #f5f5f5;
    --tab-active: #2563eb;
  }}
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: var(--bg); color: var(--fg);
    max-width: 960px; margin: 0 auto; padding: 2rem 1.5rem;
    font-size: 14px; line-height: 1.5;
  }}
  h1 {{ font-size: 1.5rem; margin-bottom: 0.25rem; }}
  h2 {{ font-size: 1.1rem; margin: 2rem 0 0.75rem; }}
  .subtitle {{ color: var(--muted); margin-bottom: 2rem; font-size: 0.85rem; }}

  table {{
    width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums;
    background: var(--card-bg); border: 1px solid var(--border); border-radius: 6px;
    overflow: hidden; margin-bottom: 1.5rem;
  }}
  th, td {{ padding: 0.6rem 1rem; text-align: right; border-bottom: 1px solid var(--border); }}
  th {{ background: var(--header-bg); font-weight: 600; font-size: 0.75rem;
       text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); }}
  th:first-child, td:first-child {{ text-align: left; }}
  tr:last-child td {{ border-bottom: none; }}

  .note {{ color: var(--muted); font-size: 0.8rem; margin-top: -1rem; margin-bottom: 1.5rem; font-style: italic; }}
  canvas {{ width: 100% !important; }}
  .chart-box {{
    background: var(--card-bg); border: 1px solid var(--border);
    border-radius: 6px; padding: 1.25rem; margin-bottom: 1.5rem;
  }}

  .tabs {{
    display: flex; gap: 0; border-bottom: 2px solid var(--border);
    margin-bottom: 1.5rem; margin-top: 1rem;
  }}
  .tab {{
    padding: 0.5rem 1.25rem; cursor: pointer; font-size: 0.85rem;
    font-weight: 500; color: var(--muted); border-bottom: 2px solid transparent;
    margin-bottom: -2px; transition: all 0.15s;
  }}
  .tab:hover {{ color: var(--fg); }}
  .tab.active {{ color: var(--fg); border-bottom-color: var(--tab-active); }}
  .tab-panel {{ display: none; }}
  .tab-panel.active {{ display: block; }}
</style>
</head>
<body>

<h1>MXC Containment Benchmark</h1>
<p class="subtitle">Comparison of Hyperlight, NanVix, and WSLc backends</p>

<div id="app"></div>

<script>
const D = {json_data};
const COLORS = ['#2563eb', '#dc2626', '#059669'];
const app = document.getElementById('app');

function h(tag, attrs, ...children) {{
  const el = document.createElement(tag);
  if (attrs) Object.entries(attrs).forEach(([k,v]) => {{
    if (k === 'className') el.className = v;
    else if (k === 'style') Object.assign(el.style, v);
    else el.setAttribute(k, v);
  }});
  children.flat().forEach(c => {{
    if (typeof c === 'string') el.appendChild(document.createTextNode(c));
    else if (c) el.appendChild(c);
  }});
  return el;
}}

function fmt(v, unit) {{
  if (v == null || v === 0) return '—';
  return v.toFixed(1) + (unit || '');
}}

function drawLatencyChart(canvas, datasets) {{
  const ctx = canvas.getContext('2d');
  const dpr = window.devicePixelRatio || 1;
  const rect = canvas.getBoundingClientRect();
  canvas.width = rect.width * dpr; canvas.height = rect.height * dpr;
  ctx.scale(dpr, dpr);
  const W = rect.width, H = rect.height;
  const pad = {{ top: 24, right: 20, bottom: 32, left: 56 }};
  const pW = W - pad.left - pad.right, pH = H - pad.top - pad.bottom;
  const allT = datasets.flatMap(d => d.values);
  if (!allT.length) return;
  const yMax = Math.max(...allT) * 1.15;
  const maxN = Math.max(...datasets.map(d => d.values.length));
  const cs = getComputedStyle(document.documentElement);
  const fg = cs.getPropertyValue('--fg').trim() || '#333';
  const grid = cs.getPropertyValue('--grid').trim() || '#eee';
  ctx.strokeStyle = grid; ctx.lineWidth = 0.5;
  for (let i = 0; i <= 4; i++) {{
    const y = pad.top + pH - (i / 4) * pH;
    ctx.beginPath(); ctx.moveTo(pad.left, y); ctx.lineTo(pad.left + pW, y); ctx.stroke();
    ctx.fillStyle = fg; ctx.font = '10px sans-serif'; ctx.textAlign = 'right';
    ctx.fillText((yMax * i / 4).toFixed(0), pad.left - 6, y + 3);
  }}
  ctx.fillStyle = fg; ctx.font = '10px sans-serif'; ctx.textAlign = 'center';
  ctx.fillText('Iteration', pad.left + pW / 2, H - 4);
  datasets.forEach((ds, di) => {{
    const color = COLORS[di % COLORS.length];
    ctx.strokeStyle = color; ctx.lineWidth = 1.5;
    ctx.beginPath();
    ds.values.forEach((v, j) => {{
      const x = pad.left + (j / (maxN - 1 || 1)) * pW;
      const y = pad.top + pH - (v / yMax) * pH;
      j === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
    }});
    ctx.stroke();
    ctx.fillStyle = color;
    ds.values.forEach((v, j) => {{
      const x = pad.left + (j / (maxN - 1 || 1)) * pW;
      const y = pad.top + pH - (v / yMax) * pH;
      ctx.beginPath(); ctx.arc(x, y, 2.5, 0, Math.PI * 2); ctx.fill();
    }});
    const lx = pad.left + 8 + di * 140, ly = pad.top + 12;
    ctx.fillStyle = color; ctx.fillRect(lx, ly - 6, 10, 10);
    ctx.fillStyle = fg; ctx.textAlign = 'left'; ctx.font = '11px sans-serif';
    ctx.fillText(ds.label, lx + 14, ly + 3);
  }});
}}

// --- Front-page summary (all workloads at a glance) ---
{{
  const allBackends = [...new Set(D.workloads.flatMap(wl =>
    wl.warm_start.filter(w => !w.error).map(w => w.backend)))];

  const sRows = [];
  D.workloads.forEach(wl => {{
    sRows.push([wl.workload + ' warm-start (ms)', ...allBackends.map(b => {{
      const r = wl.warm_start.find(w => w.backend === b);
      return r && !r.error ? fmt(r.stats.median_ms) : '—';
    }})]);
    sRows.push([wl.workload + ' cold-start (ms)', ...allBackends.map(b => {{
      const r = wl.cold_start.find(c => c.backend === b);
      return r && !r.error ? fmt(r.stats.median_ms) : '—';
    }})]);
  }});

  // Density (same across workloads, take from first)
  if (D.workloads.length) {{
    const wl0 = D.workloads[0];
    sRows.push(['Commit (MB/runner)', ...allBackends.map(b => {{
      const r = wl0.density.find(d => d.backend === b);
      if (!r) return '—';
      return r.note ? fmt(r.density_cost_mb) + ' *' : fmt(r.density_cost_mb);
    }})]);
    sRows.push(['Fits in 1.5 GB', ...allBackends.map(b => {{
      const r = wl0.density.find(d => d.backend === b);
      if (!r) return '—';
      return r.note ? String(r.fits_in_1500mb) + ' *' : String(r.fits_in_1500mb);
    }})]);
  }}

  // Parallel throughput (per-workload rows, matching warm/cold pattern)
  D.workloads.forEach(wl => {{
    if (wl.parallel && wl.parallel.length) {{
      const conc = wl.parallel[0] ? wl.parallel[0].concurrency : '?';
      sRows.push([wl.workload + ' parallel (' + conc + '×, exec/s)', ...allBackends.map(b => {{
        const r = wl.parallel.find(p => p.backend === b);
        if (!r || r.error) return '—';
        return r.throughput_per_sec.toFixed(0);
      }})]);
    }}
  }});

  // Disk
  sRows.push(['Disk footprint (MB)', ...allBackends.map(b => {{
    const r = D.disk.find(d => d.backend === b);
    if (!r) return '—';
    if (r.total_mb > 0) return r.note ? fmt(r.total_mb) + ' *' : fmt(r.total_mb);
    return r.note ? 'N/A *' : '—';
  }})]);

  const sHead = h('thead', null, h('tr', null,
    h('th', null, 'Metric'), ...allBackends.map(b => h('th', null, b))));
  const sBody = h('tbody', null,
    ...sRows.map(r => h('tr', null, ...r.map((cell, i) => h(i === 0 ? 'td' : 'td', null, cell)))));
  app.appendChild(h('h2', null, 'Summary'));
  app.appendChild(h('table', null, sHead, sBody));

  // Footnotes
  const sNotes = [];
  if (D.workloads.length) {{
    D.workloads[0].density.filter(d => d.note).forEach(d => sNotes.push(d.backend + ': ' + d.note));
  }}
  D.disk.filter(d => d.note).forEach(d => sNotes.push(d.backend + ' disk: ' + d.note));
  if (sNotes.length) {{
    app.appendChild(h('p', {{className: 'note'}}, '* ' + sNotes.join('. ')));
  }}
}}

// --- Build per-workload tabs ---
const multiWorkload = D.workloads.length > 1;
const allCanvases = [];

if (multiWorkload) {{
  const tabBar = h('div', {{className: 'tabs'}});
  D.workloads.forEach((wl, wi) => {{
    const tab = h('div', {{className: 'tab' + (wi === 0 ? ' active' : '')}}, wl.workload);
    tab.onclick = () => {{
      tabBar.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
      tab.classList.add('active');
      document.querySelectorAll('.tab-panel').forEach(p => p.classList.remove('active'));
      document.getElementById('wl-' + wi).classList.add('active');
      // redraw canvases in the newly visible panel (delay for layout reflow)
      setTimeout(() => {{
        allCanvases.filter(c => c.panel === wi).forEach(c => c.draw());
      }}, 50);
    }};
    tabBar.appendChild(tab);
  }});
  app.appendChild(tabBar);
}}

D.workloads.forEach((wl, wi) => {{
  const panel = h('div', {{
    className: 'tab-panel' + (wi === 0 || !multiWorkload ? ' active' : ''),
    id: 'wl-' + wi
  }});

  const backends = wl.warm_start.filter(w => !w.error).map(w => w.backend);

  // Warm-start chart
  const warmDs = wl.warm_start.filter(w => !w.error).map(w => ({{
    label: w.backend, values: w.iterations.map(it => it.elapsed_ms)
  }}));
  if (warmDs.length) {{
    panel.appendChild(h('h2', null, 'Warm-start latency'));
    const box = h('div', {{className: 'chart-box'}});
    const cv = h('canvas', {{height: '220'}});
    box.appendChild(cv); panel.appendChild(box);
    const draw = () => {{ if (cv.getBoundingClientRect().width > 0) drawLatencyChart(cv, warmDs); }};
    allCanvases.push({{ panel: wi, draw }});
    if (wi === 0 || !multiWorkload) setTimeout(draw, 0);
    window.addEventListener('resize', draw);
  }}

  // Cold-start chart
  const coldDs = wl.cold_start.filter(c => !c.error).map(c => ({{
    label: c.backend, values: c.entries.map(e => e.elapsed_ms)
  }}));
  if (coldDs.length) {{
    panel.appendChild(h('h2', null, 'Cold-start latency'));
    const box = h('div', {{className: 'chart-box'}});
    const cv = h('canvas', {{height: '220'}});
    box.appendChild(cv); panel.appendChild(box);
    const draw = () => {{ if (cv.getBoundingClientRect().width > 0) drawLatencyChart(cv, coldDs); }};
    allCanvases.push({{ panel: wi, draw }});
    if (wi === 0 || !multiWorkload) setTimeout(draw, 0);
    window.addEventListener('resize', draw);
  }}

  // Warm-start detail
  panel.appendChild(h('h2', null, 'Warm-start detail'));
  const wHead = h('thead', null, h('tr', null,
    ...['Backend','Median','Mean','Min','Max','P95','Stdev'].map(t => h('th', null, t))));
  const wBody = h('tbody', null, ...wl.warm_start.map(w => {{
    if (w.error) return h('tr', null, h('td', null, w.backend), h('td', {{colspan:'6'}}, w.error));
    const s = w.stats;
    return h('tr', null, h('td',null,w.backend),
      ...[s.median_ms,s.mean_ms,s.min_ms,s.max_ms,s.p95_ms,s.stdev_ms].map(v=>h('td',null,fmt(v))));
  }}));
  panel.appendChild(h('table', null, wHead, wBody));

  // Cold-start detail
  panel.appendChild(h('h2', null, 'Cold-start detail'));
  const cHead = h('thead', null, h('tr', null,
    ...['Backend','Median','Mean','Min','Max','P95','Stdev','Peak Commit'].map(t => h('th', null, t))));
  const cBody = h('tbody', null, ...wl.cold_start.map(c => {{
    if (c.error) return h('tr', null, h('td', null, c.backend), h('td', {{colspan:'7'}}, c.error));
    const s = c.stats; const peakC = Math.max(...c.entries.map(e => e.peak_commit_mb));
    return h('tr', null, h('td',null,c.backend),
      ...[s.median_ms,s.mean_ms,s.min_ms,s.max_ms,s.p95_ms,s.stdev_ms].map(v=>h('td',null,fmt(v))),
      h('td',null,fmt(peakC)+' MB'));
  }}));
  panel.appendChild(h('table', null, cHead, cBody));

  // Density detail
  panel.appendChild(h('h2', null, 'Density (memory commit)'));
  const dHead = h('thead', null, h('tr', null,
    ...['Backend','Cost (MB/runner)','Fits in 1.5 GB'].map(t => h('th', null, t))));
  const dBody = h('tbody', null, ...wl.density.map(d => {{
    return h('tr', null, h('td',null,d.backend),
      h('td',null,fmt(d.density_cost_mb)), h('td',null,String(d.fits_in_1500mb)));
  }}));
  panel.appendChild(h('table', null, dHead, dBody));

  // Parallel execution detail
  if (wl.parallel && wl.parallel.length) {{
    const validPar = wl.parallel.filter(p => !p.error);
    if (validPar.length) {{
      panel.appendChild(h('h2', null, 'Parallel execution (' + validPar[0].concurrency + '× concurrent)'));

      // Parallel chart: wall-clock per round
      const parDs = validPar.map(p => ({{
        label: p.backend,
        values: p.rounds.map(r => r.wall_clock_ms)
      }}));
      if (parDs.length) {{
        const box = h('div', {{className: 'chart-box'}});
        const cv = h('canvas', {{height: '220'}});
        box.appendChild(cv); panel.appendChild(box);
        const draw = () => {{ if (cv.getBoundingClientRect().width > 0) drawLatencyChart(cv, parDs); }};
        allCanvases.push({{ panel: wi, draw }});
        if (wi === 0 || !multiWorkload) setTimeout(draw, 0);
        window.addEventListener('resize', draw);
      }}

      const pHead = h('thead', null, h('tr', null,
        ...['Backend','Concurrency','Wall-clock (ms)','Per-runner (ms)','Throughput (exec/s)'].map(t => h('th', null, t))));
      const pBody = h('tbody', null, ...validPar.map(p => {{
        return h('tr', null,
          h('td',null,p.backend),
          h('td',null,String(p.concurrency)),
          h('td',null,fmt(p.wall_clock_stats.median_ms)),
          h('td',null,fmt(p.per_runner_stats.median_ms)),
          h('td',null,p.throughput_per_sec.toFixed(0)));
      }}));
      panel.appendChild(h('table', null, pHead, pBody));
    }}
  }}

  app.appendChild(panel);
}});

// --- Disk footprint (workload-independent, shown once outside tabs) ---
if (D.disk.some(d => d.files.length || d.note)) {{
  app.appendChild(h('h2', null, 'Disk footprint'));
  const fHead = h('thead', null, h('tr', null,
    ...['Backend','On-disk (MB)','Files / Notes'].map(t => h('th', null, t))));
  const fBody = h('tbody', null, ...D.disk.map(d => {{
    if (d.files.length) {{
      const files = d.files.map(f => f.name + ' (' + fmt(f.actual_mb) + ' MB)').join(', ');
      return h('tr', null, h('td',null,d.backend), h('td',null,fmt(d.total_mb)),
        h('td',{{style:{{textAlign:'left'}}}},files));
    }} else {{
      return h('tr', null, h('td',null,d.backend), h('td',null,'N/A'),
        h('td',{{style:{{textAlign:'left',fontStyle:'italic',color:'var(--muted)'}}}}, d.note || 'No files found'));
    }}
  }}));
  app.appendChild(h('table', null, fHead, fBody));
}}
</script>
</body>
</html>"##
    )
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    let backends: Vec<ContainmentBackend> = if cli.all || cli.full {
        vec![
            ContainmentBackend::Hyperlight,
            ContainmentBackend::MicroVm,
            ContainmentBackend::Wslc,
        ]
    } else if let Some(b) = cli.backend {
        vec![b]
    } else {
        eprintln!("Specify --backend <name>, --all, or --full");
        std::process::exit(1);
    };

    // Full benchmark mode: one command → one HTML
    if cli.full {
        let output_path = cli.output_html.as_deref().unwrap_or("bench_report.html");
        let workloads: Vec<String> = cli.workloads.clone()
            .unwrap_or_else(|| vec![cli.workload.clone()]);

        eprintln!("Running full benchmark: warm-start, cold-start, density, disk");
        eprintln!("Workloads: {}\n", workloads.join(", "));

        let mut workload_results = Vec::new();

        for wl in &workloads {
            eprintln!("\n{}", "=".repeat(60));
            eprintln!("  WORKLOAD: {wl}");
            eprintln!("{}", "=".repeat(60));

            // 1. Warm-start
            let warm_start: Vec<BackendResult> = backends
                .iter()
                .map(|b| benchmark_backend(b, cli.warmup, cli.iterations, &cli.wslc_image, None, wl))
                .collect();

            // 2. Cold-start
            let cold_start: Vec<ColdStartResult> = backends
                .iter()
                .map(|b| cold_start_benchmark(b, cli.warmup, cli.iterations, &cli.wslc_image, wl))
                .collect();

            // 3. Density (only run once per workload — memory model is workload-independent
            // but we include it per-workload so the report shows exec times)
            let density = density_test(cli.density_count, &backends, &cli.wslc_image, wl);

            // 4. Parallel (concurrent execution scaling)
            let parallel: Vec<ParallelResult> = backends
                .iter()
                .map(|b| parallel_benchmark(b, cli.parallel_count, cli.warmup, cli.iterations, &cli.wslc_image, wl))
                .collect();

            workload_results.push(WorkloadResult {
                workload: wl.clone(),
                warm_start,
                cold_start,
                density,
                parallel,
            });
        }

        // 4. Disk (workload-independent, run once)
        let disk: Vec<DiskResult> = backends.iter().map(|b| measure_disk(b, &cli.wslc_image)).collect();

        let result = FullResult { workloads: workload_results, disk };

        // Write JSON
        if let Some(path) = &cli.output_json {
            let json = serde_json::to_string_pretty(&result).expect("serialize");
            fs::write(path, &json).expect("write JSON");
            eprintln!("JSON written to {path}");
        }

        // Write HTML
        let html = generate_full_html(&result);
        fs::write(output_path, &html).expect("write HTML");
        eprintln!("\nHTML report written to {output_path}");

        // Print summary
        eprintln!("\n{}", "=".repeat(60));
        eprintln!("  FULL BENCHMARK SUMMARY");
        eprintln!("{}", "=".repeat(60));
        for wl_result in &result.workloads {
            eprintln!("\n  Workload: {}", wl_result.workload);
            for b in &backends {
                let name = backend_display_name(b);
                let warm = wl_result.warm_start.iter().find(|r| r.backend == name);
                let cold = wl_result.cold_start.iter().find(|r| r.backend == name);
                let dens = wl_result.density.iter().find(|r| r.backend == name);
                eprintln!("  {name}:");
                if let Some(w) = warm { if w.error.is_none() { eprintln!("    warm-start: {:.1}ms median", w.stats.median_ms); } }
                if let Some(c) = cold { if c.error.is_none() { eprintln!("    cold-start: {:.1}ms median  peakCommit={:.1}MB", c.stats.median_ms, c.entries.iter().map(|e| e.peak_commit_mb).fold(0.0f64, f64::max)); } }
                if let Some(d) = dens { eprintln!("    density:    {:.1}MB/runner  ~{} fit in 1.5GB", d.density_cost_mb, d.fits_in_1500mb); }
                let par = wl_result.parallel.iter().find(|r| r.backend == name);
                if let Some(p) = par { if p.error.is_none() { eprintln!("    parallel:   {:.0} exec/sec  ({}×, wall={:.1}ms)", p.throughput_per_sec, p.concurrency, p.wall_clock_stats.median_ms); } }
            }
        }
        for b in &backends {
            let name = backend_display_name(b);
            if let Some(d) = result.disk.iter().find(|r| r.backend == name) {
                if d.total_mb > 0.0 {
                    eprintln!("  {name}: disk={:.1}MB", d.total_mb);
                } else if let Some(note) = &d.note {
                    eprintln!("  {name}: disk=N/A ({note})");
                }
            }
        }

        return;
    }

    // Density test mode
    if let Some(n) = cli.density {
        let results = density_test(n, &backends, &cli.wslc_image, &cli.workload);

        // Summary
        eprintln!("\n{}", "=".repeat(60));
        eprintln!("  DENSITY SUMMARY");
        eprintln!("{}", "=".repeat(60));
        for r in &results {
            let model = if r.ephemeral { "ephemeral" } else { "persistent" };
            eprintln!(
                "  {:20} {}/{} runners  cost={:.1}MB/runner ({})  ~{} fit in 1.5GB",
                r.backend, r.count, n, r.density_cost_mb, model, r.fits_in_1500mb,
            );
        }

        if let Some(path) = &cli.output_json {
            let json = serde_json::to_string_pretty(&results).expect("serialize");
            fs::write(path, &json).expect("write JSON");
            eprintln!("\nJSON written to {path}");
        }

        println!("{}", serde_json::to_string_pretty(&results).unwrap());
        return;
    }

    // Normal benchmark mode
    let custom_config = cli.config.as_deref();
    let mut all_results = Vec::new();

    for backend in &backends {
        let result = benchmark_backend(
            backend,
            cli.warmup,
            cli.iterations,
            &cli.wslc_image,
            custom_config,
            &cli.workload,
        );
        all_results.push(result);
    }

    // Summary
    eprintln!("\n{}", "=".repeat(60));
    eprintln!(
        "  SUMMARY (library-mode, steady-state, workload: {})",
        cli.workload
    );
    eprintln!("{}", "=".repeat(60));
    for r in &all_results {
        if let Some(err) = &r.error {
            eprintln!("  {:20} ERROR: {err}", r.backend);
        } else {
            eprintln!(
                "  {:20} median={:.1}ms  mean={:.1}ms  (setup={:.0}ms)",
                r.backend,
                r.stats.median_ms,
                r.stats.mean_ms,
                r.runner_create_ms.unwrap_or(0.0)
            );
        }
    }

    // JSON output
    if let Some(path) = &cli.output_json {
        let json = serde_json::to_string_pretty(&all_results).expect("serialize");
        fs::write(path, &json).expect("write JSON");
        eprintln!("\nJSON written to {path}");
    }

    // HTML output
    if let Some(path) = &cli.output_html {
        let html = generate_html(&all_results);
        fs::write(path, &html).expect("write HTML");
        eprintln!("HTML written to {path}");
    }

    // Also print JSON to stdout for piping
    println!("{}", serde_json::to_string_pretty(&all_results).unwrap());
}
