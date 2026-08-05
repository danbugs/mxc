// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Library-mode benchmark for MXC containment backends.
//!
//! Unlike the CLI benchmark (which spawns `wxc-exec` per iteration and pays
//! process-creation + config-parse overhead every time), this binary creates
//! the runner **once** and calls `execute()` in a loop, measuring the
//! steady-state latency that an in-process caller (e.g. the mxc SDK) sees.
//!
//! Supports: Hyperlight, MicroVM (NanVix), WSLc.
//!
//! Usage:
//!   bench-library --backend hyperlight --iterations 20 --warmup 3
//!   bench-library --backend microvm --iterations 20
//!   bench-library --backend wslc --iterations 20 --wslc-image python:3.12-alpine
//!   bench-library --all --iterations 10 --output-json results.json
//!   bench-library --all --workload compute --iterations 10
//!   bench-library --density 8 --backend hyperlight

use std::fs;
use std::time::Instant;

use clap::Parser;
use serde::Serialize;
use wxc_common::config_parser::load_request;
use wxc_common::logger::{Logger, Mode};
use wxc_common::models::{ContainmentBackend, ExecutionRequest, ScriptResponse};

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

// ---------------------------------------------------------------------------
// Process memory measurement (cross-platform, no external crates)
// ---------------------------------------------------------------------------

/// Returns the current process working set in MB.
#[cfg(target_os = "windows")]
fn process_working_set_mb() -> f64 {
    use std::mem;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct ProcessMemoryCounters {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
    }

    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            ppsmem_counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    unsafe {
        let handle = GetCurrentProcess();
        let mut counters: ProcessMemoryCounters = mem::zeroed();
        counters.cb = mem::size_of::<ProcessMemoryCounters>() as u32;
        if K32GetProcessMemoryInfo(handle, &mut counters, counters.cb) != 0 {
            counters.WorkingSetSize as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        }
    }
}

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
    process_ws_mb: f64,
}

#[derive(Debug, Clone, Serialize)]
struct DensityResult {
    backend: String,
    count: usize,
    entries: Vec<DensityEntry>,
    baseline_ws_mb: f64,
    final_ws_mb: f64,
    per_runner_mb: f64,
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
        let request = make_request(backend, wslc_image, None, workload);

        eprintln!("\n{}", "=".repeat(60));
        eprintln!("  Density test: {name} × {n} runners");
        eprintln!("{}", "=".repeat(60));

        let baseline_ws = process_working_set_mb();
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

            // Execute once to force full initialization
            let t_exec = Instant::now();
            let resp = runners
                .last_mut()
                .unwrap()
                .runner
                .execute(&request, &mut Logger::new(Mode::Buffer));
            let exec_ms = t_exec.elapsed().as_secs_f64() * 1000.0;

            let ws = process_working_set_mb();
            eprintln!(
                "  [{name}] runner {}/{n}: create={create_ms:.1}ms  exec={exec_ms:.1}ms  exit={}  WS={ws:.1}MB",
                i + 1,
                resp.exit_code
            );

            entries.push(DensityEntry {
                index: i + 1,
                create_ms,
                execute_ms: exec_ms,
                exit_code: resp.exit_code,
                process_ws_mb: ws,
            });
        }

        let final_ws = process_working_set_mb();
        let alive = runners.len();
        let per_runner = if alive > 0 {
            (final_ws - baseline_ws) / alive as f64
        } else {
            0.0
        };

        eprintln!("  [{name}] {alive}/{n} runners alive");
        eprintln!("  [{name}] Final WS: {final_ws:.1} MB  (baseline: {baseline_ws:.1} MB)");
        eprintln!(
            "  [{name}] Per-runner overhead: {per_runner:.1} MB  (total delta: {:.1} MB)",
            final_ws - baseline_ws
        );

        // Check against Stuart's target: ≤128 MB per sandbox, ≥12 in 1.5 GB
        let fits_in_1500mb = ((1500.0 - baseline_ws) / per_runner).floor() as usize;
        eprintln!(
            "  [{name}] At {per_runner:.1} MB/runner: ~{fits_in_1500mb} fit in 1.5 GB budget"
        );

        results.push(DensityResult {
            backend: name.to_string(),
            count: alive,
            entries,
            baseline_ws_mb: baseline_ws,
            final_ws_mb: final_ws,
            per_runner_mb: per_runner,
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
            eprintln!(
                "  {:20} {}/{} runners  per-runner={:.1}MB  total={:.1}MB  (~{} fit in 1.5GB)",
                r.backend,
                r.count,
                n,
                r.per_runner_mb,
                r.final_ws_mb - r.baseline_ws_mb,
                ((1500.0 - r.baseline_ws_mb) / r.per_runner_mb).floor() as usize,
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
