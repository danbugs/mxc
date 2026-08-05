// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Library-mode benchmark for MXC containment backends.
//!
//! Measures steady-state per-invocation latency (runner reuse, no process
//! overhead) and per-runner memory density. For daemon-backed backends
//! (NanVix, WSLc) where VM/container memory is ephemeral or lives in external
//! processes, we measure peak WS during execution to capture the true per-VM cost.
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
    /// Ignored when --config is set.
    #[arg(long, default_value = "hello")]
    workload: String,

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

    /// Returns the current process working set in MB.
    pub fn process_working_set_mb() -> f64 {
        unsafe {
            let handle = GetCurrentProcess();
            let mut c: ProcessMemoryCounters = mem::zeroed();
            c.cb = mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut c, c.cb) != 0 {
                c.WorkingSetSize as f64 / (1024.0 * 1024.0)
            } else {
                0.0
            }
        }
    }

    /// Returns the total working set (MB) of all processes matching any given name.
    /// Uses Win32 toolhelp snapshot — no PowerShell overhead.
    pub fn external_process_ws_mb(target_names: &[&str]) -> f64 {
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

            let mut total_ws: f64 = 0.0;

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
                                total_ws += c.WorkingSetSize as f64;
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
            total_ws / (1024.0 * 1024.0)
        }
    }

    /// Returns the peak working set (MB) of a process given its raw HANDLE.
    /// Used to query PeakWorkingSet64 of a child process after it exits
    /// (handle remains valid until closed).
    pub fn process_peak_ws_by_handle(handle: isize) -> f64 {
        unsafe {
            let mut c: ProcessMemoryCounters = mem::zeroed();
            c.cb = mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut c, c.cb) != 0 {
                c.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
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
use mem_win::{external_process_ws_mb, file_actual_size_mb, process_peak_ws_by_handle, process_working_set_mb};

#[cfg(not(target_os = "windows"))]
fn process_working_set_mb() -> f64 {
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
fn external_process_ws_mb(_target_names: &[&str]) -> f64 {
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

/// Python source for the "compute" workload (~100-200ms CPU-bound).
/// Fibonacci is a pure-Python CPU benchmark with no dependencies.
const COMPUTE_PY: &str = r#"import sys, time
t0 = time.time()
a, b = 0, 1
for _ in range(200000):
    a, b = b, a + b
elapsed_ms = (time.time() - t0) * 1000
print(f'fib(200000): {len(str(a))} digits in {elapsed_ms:.0f}ms')
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
        ContainmentBackend::Wslc => format!("python3 -c \"{}\"", py_src.replace('\n', "; ")),
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
    /// Process WS after execute returns (persistent memory).
    process_ws_mb: f64,
    /// Peak process WS polled during execute (captures ephemeral VM memory).
    peak_ws_during_exec_mb: f64,
    /// Peak external daemon WS during execute (captures nanvixd subprocess).
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_daemon_ws_mb: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct DensityResult {
    backend: String,
    count: usize,
    /// Whether VM/container memory is ephemeral (freed after execute).
    ephemeral: bool,
    entries: Vec<DensityEntry>,
    baseline_ws_mb: f64,
    final_ws_mb: f64,
    /// Persistent per-runner overhead (WS that stays after execute).
    per_runner_persistent_mb: f64,
    /// Peak per-execution overhead (WS during execute, includes ephemeral VM).
    per_exec_peak_mb: f64,
    /// External daemon memory growth.
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_daemon_ws_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_daemon_ws_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_runner_daemon_mb: Option<f64>,
    /// The cost used for density estimation:
    /// - Persistent backends (Hyperlight): persistent per-runner + daemon
    /// - Ephemeral backends (NanVix/WSLc): peak per-execution + daemon
    density_cost_mb: f64,
    fits_in_1500mb: usize,
}

/// Polls process WS and external daemon WS in a background thread during
/// execute(). Returns (ScriptResponse, exec_ms, peak_process_ws_mb, peak_daemon_ws_mb).
///
/// For NanVix, nanvixd.exe is spawned as a short-lived subprocess during
/// execute() — this captures its WS while it's alive.
fn execute_with_peak_ws(
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
            let ws = process_working_set_mb();
            {
                let mut p = peak_proc_c.lock().unwrap();
                if ws > *p { *p = ws; }
            }
            if !names.is_empty() {
                let dws = external_process_ws_mb(&names);
                let mut p = peak_daemon_c.lock().unwrap();
                if dws > *p { *p = dws; }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    let t = Instant::now();
    let resp = runner.execute(request, &mut Logger::new(Mode::Buffer));
    let exec_ms = t.elapsed().as_secs_f64() * 1000.0;

    running.store(false, Ordering::Relaxed);
    poller.join().unwrap();

    let peak_ws = *peak_proc.lock().unwrap();
    let peak_dws = *peak_daemon.lock().unwrap();
    (resp, exec_ms, peak_ws, peak_dws)
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

        let baseline_ws = process_working_set_mb();
        let baseline_daemon_ws = if has_daemon {
            let ws = external_process_ws_mb(&dnames);
            eprintln!("  Baseline daemon WS: {ws:.1} MB");
            Some(ws)
        } else {
            None
        };
        eprintln!("  Baseline process WS: {baseline_ws:.1} MB");

        let mut runners = Vec::with_capacity(n);
        let mut entries = Vec::with_capacity(n);

        for i in 0..n {
            // Create runner
            let t_create = Instant::now();
            let mut logger = Logger::new(Mode::Buffer);
            let resolved = match mxc_engine::resolve_runner(&request, &mut logger) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("  [{name}] runner {}/{n} FAILED: {e}", i + 1);
                    break;
                }
            };
            let create_ms = t_create.elapsed().as_secs_f64() * 1000.0;
            runners.push(resolved);

            // Execute once with peak WS polling (also polls daemon WS during execute)
            let daemon_names_owned: Vec<String> = dnames.iter().map(|s| s.to_string()).collect();
            let (resp, exec_ms, peak_ws, peak_dws) = execute_with_peak_ws(
                runners.last_mut().unwrap().runner.as_mut(),
                &request,
                daemon_names_owned,
            );

            let ws = process_working_set_mb();

            if has_daemon {
                eprintln!(
                    "  [{name}] runner {}/{n}: create={create_ms:.1}ms  exec={exec_ms:.1}ms  exit={}  WS={ws:.1}MB  peakWS={peak_ws:.1}MB  peakDaemon={peak_dws:.1}MB",
                    i + 1,
                    resp.exit_code,
                );
            } else {
                eprintln!(
                    "  [{name}] runner {}/{n}: create={create_ms:.1}ms  exec={exec_ms:.1}ms  exit={}  WS={ws:.1}MB  peakWS={peak_ws:.1}MB",
                    i + 1,
                    resp.exit_code,
                );
            }

            entries.push(DensityEntry {
                index: i + 1,
                create_ms,
                execute_ms: exec_ms,
                exit_code: resp.exit_code,
                process_ws_mb: ws,
                peak_ws_during_exec_mb: peak_ws,
                peak_daemon_ws_mb: if has_daemon { Some(peak_dws) } else { None },
            });
        }

        let final_ws = process_working_set_mb();
        let final_daemon_ws = if has_daemon {
            Some(external_process_ws_mb(&dnames))
        } else {
            None
        };

        let alive = runners.len();
        let per_runner_persistent = if alive > 0 {
            (final_ws - baseline_ws) / alive as f64
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
                        baseline_ws
                    } else {
                        entries[i - 1].process_ws_mb
                    };
                    (e.peak_ws_during_exec_mb - before).max(0.0)
                })
                .collect();
            deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
            deltas[deltas.len() / 2] // median
        } else {
            0.0
        };

        // Daemon cost depends on whether it's a subprocess or persistent service:
        // - nanvixd: short-lived subprocess (baseline WS = 0). Per-exec cost = peak WS.
        // - wslservice: persistent service (baseline WS > 0). Per-exec cost = peak - baseline.
        let per_runner_daemon = if has_daemon && !entries.is_empty() {
            let baseline_dws = baseline_daemon_ws.unwrap_or(0.0);
            let mut deltas: Vec<f64> = entries
                .iter()
                .filter_map(|e| e.peak_daemon_ws_mb.map(|p| (p - baseline_dws).max(0.0)))
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

        // Choose the right cost metric for density estimation
        let density_cost = if ephemeral {
            // Ephemeral: per-execution peak (process WS) + daemon subprocess cost
            per_exec_peak + per_runner_daemon.unwrap_or(0.0)
        } else {
            // Persistent: per-runner WS (snapshot stays in memory) + daemon
            per_runner_persistent + per_runner_daemon.unwrap_or(0.0)
        };

        let fits = if density_cost > 0.0 {
            ((1500.0 - baseline_ws) / density_cost).floor() as usize
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
        if let (Some(d), Some(b)) = (per_runner_daemon, baseline_daemon_ws) {
            let label = if b < 0.1 { "subprocess" } else { "service delta" };
            eprintln!(
                "  [{name}] Daemon ({label}):      {d:.1} MB/exec  (baseline={b:.1}MB)"
            );
        }
        eprintln!(
            "  [{name}] => Density cost: {density_cost:.1} MB/runner  (~{fits} fit in 1.5 GB)"
        );

        results.push(DensityResult {
            backend: name.to_string(),
            count: alive,
            ephemeral,
            entries,
            baseline_ws_mb: baseline_ws,
            final_ws_mb: final_ws,
            per_runner_persistent_mb: per_runner_persistent,
            per_exec_peak_mb: per_exec_peak,
            daemon_names: if has_daemon {
                Some(dnames.iter().map(|s| s.to_string()).collect())
            } else {
                None
            },
            baseline_daemon_ws_mb: baseline_daemon_ws,
            final_daemon_ws_mb: final_daemon_ws,
            per_runner_daemon_mb: per_runner_daemon,
            density_cost_mb: density_cost,
            fits_in_1500mb: fits,
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
    peak_ws_mb: f64,
}

#[derive(Debug, Clone, Serialize)]
struct ColdStartResult {
    backend: String,
    stats: Stats,
    entries: Vec<ColdStartEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Write a temp wxc-exec config for the given backend + workload.
fn write_temp_config(
    backend: &ContainmentBackend,
    wslc_image: &str,
    workload: &str,
) -> std::path::PathBuf {
    let py_src = workload_py_src(workload);
    let json = match backend {
        ContainmentBackend::Hyperlight => format!(
            r#"{{"process":{{"commandLine":"{}","timeout":30000}},"containment":"hyperlight"}}"#,
            py_src.replace('\\', "\\\\").replace('"', "\\\"")
        ),
        ContainmentBackend::MicroVm => format!(
            r#"{{"process":{{"commandLine":"{}","timeout":30000}},"containment":"microvm"}}"#,
            py_src.replace('\\', "\\\\").replace('"', "\\\"")
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
        let peak_ws = {
            use std::os::windows::io::AsRawHandle;
            process_peak_ws_by_handle(child.as_raw_handle() as isize)
        };
        #[cfg(not(target_os = "windows"))]
        let peak_ws = 0.0;

        Some((elapsed_ms, exit_code, peak_ws))
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
            Some((ms, exit, peak_ws)) => {
                times.push(ms);
                entries.push(ColdStartEntry {
                    iteration: i + 1,
                    elapsed_ms: ms,
                    exit_code: exit,
                    peak_ws_mb: peak_ws,
                });
                eprintln!("  [{}/{}] {ms:.1}ms exit={exit} peakWS={peak_ws:.1}MB", i + 1, iterations);
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
        "  => median={:.1}ms  p99={:.1}ms  peakWS={:.1}MB",
        stats.median_ms, stats.p95_ms,
        entries.iter().map(|e| e.peak_ws_mb).fold(0.0f64, f64::max)
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
}

#[derive(Debug, Clone, Serialize)]
struct DiskFile {
    name: String,
    logical_mb: f64,
    actual_mb: f64,
}

fn measure_disk(backend: &ContainmentBackend) -> DiskResult {
    let name = backend_display_name(backend);
    let exe_dir = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("exe parent")
        .to_path_buf();

    let file_list: Vec<std::path::PathBuf> = match backend {
        ContainmentBackend::Hyperlight => vec![
            exe_dir.join("snapshots").join("kernel.vmem"),
            exe_dir.join("snapshots").join("kernel.whp.cbor"),
        ],
        ContainmentBackend::MicroVm => vec![
            exe_dir.join("nanvixd.exe"),
            exe_dir.join("nanvix_rootfs.img"),
            exe_dir.join("python3.initrd"),
        ],
        ContainmentBackend::Wslc => vec![],  // OCI cache, hard to measure
        _ => vec![],
    };

    let mut files = Vec::new();
    let mut total = 0.0;

    for path in &file_list {
        if path.exists() {
            let logical = path.metadata().map(|m| m.len() as f64 / (1024.0 * 1024.0)).unwrap_or(0.0);
            #[cfg(target_os = "windows")]
            let actual = file_actual_size_mb(path);
            #[cfg(not(target_os = "windows"))]
            let actual = logical;
            total += actual;
            files.push(DiskFile {
                name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                logical_mb: logical,
                actual_mb: actual,
            });
        }
    }

    DiskResult { backend: name.to_string(), total_mb: total, files }
}

// ---------------------------------------------------------------------------
// Full benchmark (one command → one HTML)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct FullResult {
    warm_start: Vec<BackendResult>,
    cold_start: Vec<ColdStartResult>,
    density: Vec<DensityResult>,
    disk: Vec<DiskResult>,
}

fn generate_full_html(result: &FullResult) -> String {
    let json_data = serde_json::to_string(&result).unwrap_or_default();

    // Build the scorecard rows from data
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
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --bg: #0d1117; --fg: #e6edf3; --card-bg: #161b22; --border: #30363d;
      --accent1: #58a6ff; --accent2: #f85149; --accent3: #3fb950;
      --grid: #21262d; --muted: #8b949e; --header-bg: #1c2128;
    }}
  }}
  :root[data-theme="dark"] {{
    --bg: #0d1117; --fg: #e6edf3; --card-bg: #161b22; --border: #30363d;
    --accent1: #58a6ff; --accent2: #f85149; --accent3: #3fb950;
    --grid: #21262d; --muted: #8b949e; --header-bg: #1c2128;
  }}
  :root[data-theme="light"] {{
    --bg: #fafafa; --fg: #1a1a2e; --card-bg: #fff; --border: #e0e0e0;
    --accent1: #2563eb; --accent2: #dc2626; --accent3: #059669;
    --grid: #f0f0f0; --muted: #666; --header-bg: #f5f5f5;
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

  .note {{ color: var(--muted); font-size: 0.8rem; margin-top: -1rem; margin-bottom: 1.5rem; }}
  canvas {{ width: 100% !important; }}
  .chart-box {{
    background: var(--card-bg); border: 1px solid var(--border);
    border-radius: 6px; padding: 1.25rem; margin-bottom: 1.5rem;
  }}
  .chart-title {{ font-size: 0.9rem; font-weight: 600; margin-bottom: 0.75rem; }}
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

// --- Summary table ---
const backends = D.warm_start.filter(w => !w.error).map(w => w.backend);
const rows = [];

// Cold-start
rows.push(['Cold-start median (ms)', ...backends.map(b => {{
  const r = D.cold_start.find(c => c.backend === b);
  return r && !r.error ? fmt(r.stats.median_ms) : '—';
}})]);
rows.push(['Cold-start p95 (ms)', ...backends.map(b => {{
  const r = D.cold_start.find(c => c.backend === b);
  return r && !r.error ? fmt(r.stats.p95_ms) : '—';
}})]);

// Warm-start
rows.push(['Warm-start median (ms)', ...backends.map(b => {{
  const r = D.warm_start.find(w => w.backend === b);
  return r && !r.error ? fmt(r.stats.median_ms) : '—';
}})]);
rows.push(['Warm-start p95 (ms)', ...backends.map(b => {{
  const r = D.warm_start.find(w => w.backend === b);
  return r && !r.error ? fmt(r.stats.p95_ms) : '—';
}})]);

// Memory
rows.push(['Per-process WS peak (MB)', ...backends.map(b => {{
  const r = D.cold_start.find(c => c.backend === b);
  if (!r || r.error) return '—';
  const peak = Math.max(...r.entries.map(e => e.peak_ws_mb));
  return fmt(peak);
}})]);
rows.push(['Density cost (MB/runner)', ...backends.map(b => {{
  const r = D.density.find(d => d.backend === b);
  return r ? fmt(r.density_cost_mb) : '—';
}})]);
rows.push(['Density: fits in 1.5 GB', ...backends.map(b => {{
  const r = D.density.find(d => d.backend === b);
  return r ? String(r.fits_in_1500mb) : '—';
}})]);

// Disk
rows.push(['Disk footprint (MB)', ...backends.map(b => {{
  const r = D.disk.find(d => d.backend === b);
  return r && r.total_mb > 0 ? fmt(r.total_mb) : '—';
}})]);

const thead = h('thead', null, h('tr', null,
  h('th', null, 'Metric'),
  ...backends.map(b => h('th', null, b))
));
const tbody = h('tbody', null,
  ...rows.map(r => h('tr', null, ...r.map((cell, i) => h(i === 0 ? 'td' : 'td', null, cell))))
);
app.appendChild(h('h2', null, 'Summary'));
app.appendChild(h('table', null, thead, tbody));

// --- Latency charts ---
function drawLatencyChart(canvas, datasets, title) {{
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

  // Y grid
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
    // Legend
    const lx = pad.left + 8 + di * 140, ly = pad.top + 12;
    ctx.fillStyle = color; ctx.fillRect(lx, ly - 6, 10, 10);
    ctx.fillStyle = fg; ctx.textAlign = 'left'; ctx.font = '11px sans-serif';
    ctx.fillText(ds.label, lx + 14, ly + 3);
  }});
}}

// Warm-start chart
const warmDs = D.warm_start.filter(w => !w.error).map(w => ({{
  label: w.backend, values: w.iterations.map(it => it.elapsed_ms)
}}));
if (warmDs.length) {{
  app.appendChild(h('h2', null, 'Warm-start latency'));
  const box = h('div', {{className: 'chart-box'}});
  const cv = h('canvas', {{height: '220'}});
  box.appendChild(cv); app.appendChild(box);
  setTimeout(() => drawLatencyChart(cv, warmDs, 'Warm-start'), 0);
  window.addEventListener('resize', () => drawLatencyChart(cv, warmDs, 'Warm-start'));
}}

// Cold-start chart
const coldDs = D.cold_start.filter(c => !c.error).map(c => ({{
  label: c.backend, values: c.entries.map(e => e.elapsed_ms)
}}));
if (coldDs.length) {{
  app.appendChild(h('h2', null, 'Cold-start latency'));
  const box = h('div', {{className: 'chart-box'}});
  const cv = h('canvas', {{height: '220'}});
  box.appendChild(cv); app.appendChild(box);
  setTimeout(() => drawLatencyChart(cv, coldDs, 'Cold-start'), 0);
  window.addEventListener('resize', () => drawLatencyChart(cv, coldDs, 'Cold-start'));
}}

// --- Detailed tables ---
app.appendChild(h('h2', null, 'Warm-start detail'));
const wHead = h('thead', null, h('tr', null, ...['Backend','Median','Mean','Min','Max','P95','Stdev','Setup'].map(t => h('th', null, t))));
const wBody = h('tbody', null, ...D.warm_start.map(w => {{
  if (w.error) return h('tr', null, h('td', null, w.backend), h('td', {{colspan:'7'}}, w.error));
  const s = w.stats;
  return h('tr', null, h('td',null,w.backend), ...[s.median_ms,s.mean_ms,s.min_ms,s.max_ms,s.p95_ms,s.stdev_ms].map(v=>h('td',null,fmt(v))), h('td',null,fmt(w.runner_create_ms||0)+'ms'));
}}));
app.appendChild(h('table', null, wHead, wBody));

app.appendChild(h('h2', null, 'Cold-start detail'));
const cHead = h('thead', null, h('tr', null, ...['Backend','Median','Mean','Min','Max','P95','Stdev','Peak WS'].map(t => h('th', null, t))));
const cBody = h('tbody', null, ...D.cold_start.map(c => {{
  if (c.error) return h('tr', null, h('td', null, c.backend), h('td', {{colspan:'7'}}, c.error));
  const s = c.stats; const peakWs = Math.max(...c.entries.map(e => e.peak_ws_mb));
  return h('tr', null, h('td',null,c.backend), ...[s.median_ms,s.mean_ms,s.min_ms,s.max_ms,s.p95_ms,s.stdev_ms].map(v=>h('td',null,fmt(v))), h('td',null,fmt(peakWs)+' MB'));
}}));
app.appendChild(h('table', null, cHead, cBody));

app.appendChild(h('h2', null, 'Density'));
const dHead = h('thead', null, h('tr', null, ...['Backend','Model','Cost (MB)','Persistent','Peak exec','Daemon','Fits 1.5GB'].map(t => h('th', null, t))));
const dBody = h('tbody', null, ...D.density.map(d => {{
  const model = d.ephemeral ? 'ephemeral' : 'persistent';
  const daemon = d.per_runner_daemon_mb != null ? fmt(d.per_runner_daemon_mb) : '—';
  return h('tr', null, h('td',null,d.backend), h('td',null,model),
    h('td',null,fmt(d.density_cost_mb)), h('td',null,fmt(d.per_runner_persistent_mb)),
    h('td',null,fmt(d.per_exec_peak_mb)), h('td',null,daemon), h('td',null,String(d.fits_in_1500mb)));
}}));
app.appendChild(h('table', null, dHead, dBody));

if (D.disk.some(d => d.files.length)) {{
  app.appendChild(h('h2', null, 'Disk footprint'));
  const fHead = h('thead', null, h('tr', null, ...['Backend','Total (MB)','Files'].map(t => h('th', null, t))));
  const fBody = h('tbody', null, ...D.disk.filter(d => d.files.length).map(d => {{
    const files = d.files.map(f => `${{f.name}} (${{fmt(f.actual_mb)}} MB)`).join(', ');
    return h('tr', null, h('td',null,d.backend), h('td',null,fmt(d.total_mb)), h('td',{{style:{{textAlign:'left'}}}},files));
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

        eprintln!("Running full benchmark: warm-start, cold-start, density, disk\n");

        // 1. Warm-start
        let warm_start: Vec<BackendResult> = backends
            .iter()
            .map(|b| benchmark_backend(b, cli.warmup, cli.iterations, &cli.wslc_image, None, &cli.workload))
            .collect();

        // 2. Cold-start
        let cold_start: Vec<ColdStartResult> = backends
            .iter()
            .map(|b| cold_start_benchmark(b, cli.warmup, cli.iterations, &cli.wslc_image, &cli.workload))
            .collect();

        // 3. Density
        let density = density_test(cli.density_count, &backends, &cli.wslc_image, &cli.workload);

        // 4. Disk
        let disk: Vec<DiskResult> = backends.iter().map(|b| measure_disk(b)).collect();

        let result = FullResult { warm_start, cold_start, density, disk };

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
        for b in &backends {
            let name = backend_display_name(b);
            let warm = result.warm_start.iter().find(|r| r.backend == name);
            let cold = result.cold_start.iter().find(|r| r.backend == name);
            let dens = result.density.iter().find(|r| r.backend == name);
            let dsk = result.disk.iter().find(|r| r.backend == name);
            eprintln!("  {name}:");
            if let Some(w) = warm { if w.error.is_none() { eprintln!("    warm-start: {:.1}ms median", w.stats.median_ms); } }
            if let Some(c) = cold { if c.error.is_none() { eprintln!("    cold-start: {:.1}ms median  peakWS={:.1}MB", c.stats.median_ms, c.entries.iter().map(|e| e.peak_ws_mb).fold(0.0f64, f64::max)); } }
            if let Some(d) = dens { eprintln!("    density:    {:.1}MB/runner  ~{} fit in 1.5GB", d.density_cost_mb, d.fits_in_1500mb); }
            if let Some(d) = dsk { if d.total_mb > 0.0 { eprintln!("    disk:       {:.1}MB", d.total_mb); } }
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
