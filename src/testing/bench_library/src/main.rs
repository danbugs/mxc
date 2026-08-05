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
}

#[cfg(target_os = "windows")]
use mem_win::{external_process_ws_mb, process_working_set_mb};

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
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    let backends: Vec<ContainmentBackend> = if cli.all {
        vec![
            ContainmentBackend::Hyperlight,
            ContainmentBackend::MicroVm,
            ContainmentBackend::Wslc,
        ]
    } else if let Some(b) = cli.backend {
        vec![b]
    } else {
        eprintln!("Specify --backend <name> or --all");
        std::process::exit(1);
    };

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
