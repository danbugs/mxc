<#
.SYNOPSIS
    Generate a complete MXC containment benchmark report.

.DESCRIPTION
    Single entry point that builds, benchmarks, and generates one HTML report
    covering everything from Stuart's micro-VM architecture doc:

    1. Cold-start latency (wxc-exec subprocess per iteration)
    2. Warm-start latency (library mode, runner reuse)
    3. Per-VM memory (peak working set)
    4. Density (N concurrent runners, per-runner MB)
    5. Disk footprint (sparse-aware)

.PARAMETER Backends
    Comma-separated: hyperlight, microvm, wslc. Default: all.

.PARAMETER Iterations
    Iterations per backend per mode. Default: 10.

.PARAMETER DensityCount
    Number of concurrent runners for density test. Default: 8.

.PARAMETER SkipBuild
    Skip cargo build step.

.PARAMETER SkipSetup
    Skip backend setup (snapshot restore, daemon start).

.PARAMETER Output
    Path for the HTML report. Default: bench_report.html.

.PARAMETER WslcImage
    Container image for WSLc. Default: python:3.12-alpine.

.EXAMPLE
    .\bench_report.ps1
    .\bench_report.ps1 -Backends hyperlight,microvm -Iterations 20
    .\bench_report.ps1 -SkipBuild -SkipSetup -Output results.html
#>

[CmdletBinding()]
param(
    [string]$Backends = "hyperlight,microvm,wslc",
    [int]$Iterations = 10,
    [int]$DensityCount = 8,
    [switch]$SkipBuild,
    [switch]$SkipSetup,
    [string]$Output = "bench_report.html",
    [string]$WslcImage = "python:3.12-alpine"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$srcDir = Join-Path $repoRoot "src"
$wxcExe = Join-Path $srcDir "target\release\wxc-exec.exe"
$benchExe = Join-Path $srcDir "target\release\bench-library.exe"
$tempDir = Join-Path ([IO.Path]::GetTempPath()) "mxc-bench-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

Write-Host "`n====================================" -ForegroundColor Cyan
Write-Host "  MXC Containment Benchmark Report" -ForegroundColor Cyan
Write-Host "====================================" -ForegroundColor Cyan
Write-Host "  Backends:  $Backends"
Write-Host "  Iterations: $Iterations"
Write-Host "  Density:   $DensityCount runners"
Write-Host "  Output:    $Output"
Write-Host ""

# ── Build ──
if (-not $SkipBuild) {
    Write-Host "[1/5] Building wxc-exec and bench-library..." -ForegroundColor Yellow
    Push-Location $srcDir
    cargo build --release -p wxc-exec --features hyperlight,microvm,wslc 2>&1 | Out-Null
    cargo build --release -p bench_library 2>&1 | Out-Null
    Pop-Location
    Write-Host "  Built." -ForegroundColor Green
} else {
    Write-Host "[1/5] Skipping build." -ForegroundColor DarkGray
}

if (-not (Test-Path $wxcExe)) { Write-Error "wxc-exec.exe not found at $wxcExe" }
if (-not (Test-Path $benchExe)) { Write-Error "bench-library.exe not found at $benchExe" }

# ── Setup ──
if (-not $SkipSetup) {
    Write-Host "[2/5] Setting up backends..." -ForegroundColor Yellow
    if ($Backends -match "hyperlight") {
        Write-Host "  Setting up Hyperlight snapshot..."
        $p = Start-Process -FilePath $wxcExe -ArgumentList "--setup-hyperlight" `
            -NoNewWindow -PassThru -Wait
        if ($p.ExitCode -ne 0) { Write-Warning "Hyperlight setup returned exit code $($p.ExitCode)" }
    }
    if ($Backends -match "microvm") {
        Write-Host "  Checking nanvixd..."
        $nanvixd = Get-Process nanvixd -ErrorAction SilentlyContinue
        if (-not $nanvixd) {
            Write-Host "  Starting nanvixd..."
            $p = Start-Process -FilePath $wxcExe -ArgumentList "--setup-nanvix" `
                -NoNewWindow -PassThru -Wait
        }
    }
    Write-Host "  Setup complete." -ForegroundColor Green
} else {
    Write-Host "[2/5] Skipping setup." -ForegroundColor DarkGray
}

# ── CLI bench (cold start + memory) ──
Write-Host "[3/5] Cold-start benchmark (wxc-exec subprocess)..." -ForegroundColor Yellow
$cliJson = Join-Path $tempDir "cli.json"
& (Join-Path $PSScriptRoot "bench_containments.ps1") `
    -Backends $Backends -Iterations $Iterations -WarmupIterations 1 `
    -SkipSetup -WxcExe $wxcExe -WslcImage $WslcImage `
    -OutputJson $cliJson
Write-Host "  CLI results: $cliJson" -ForegroundColor Green

# ── Library bench (warm start) ──
Write-Host "[4/5] Warm-start benchmark (library mode, runner reuse)..." -ForegroundColor Yellow
$libJson = Join-Path $tempDir "library.json"
$libArgs = "--all --iterations $Iterations --warmup 3 --wslc-image `"$WslcImage`" --output-json `"$libJson`""
if ($Backends -notmatch "wslc") {
    # If not all backends, specify which ones
    $backendList = $Backends -split ","
    $libArgs = ""
    foreach ($b in $backendList) {
        $b = $b.Trim()
        # bench-library needs separate runs for each backend when not --all
        $thisJson = Join-Path $tempDir "library_$b.json"
        & $benchExe --backend $b --iterations $Iterations --warmup 3 --wslc-image $WslcImage --output-json $thisJson 2>&1 | Out-Null
    }
}
# Simplify: just use --all
& $benchExe --all --iterations $Iterations --warmup 3 --wslc-image $WslcImage --output-json $libJson 2>&1 | ForEach-Object { if ($_ -match "median=|ERROR|SUMMARY") { Write-Host "  $_" } }
Write-Host "  Library results: $libJson" -ForegroundColor Green

# ── Density test ──
Write-Host "[5/5] Density test ($DensityCount concurrent runners)..." -ForegroundColor Yellow
$densityJson = Join-Path $tempDir "density.json"
& $benchExe --density $DensityCount --all --wslc-image $WslcImage --output-json $densityJson 2>&1 | ForEach-Object { if ($_ -match "runner |Per-runner|fit in") { Write-Host "  $_" } }
Write-Host "  Density results: $densityJson" -ForegroundColor Green

# ── Load results ──
$cliData = Get-Content $cliJson -Raw | ConvertFrom-Json
$libData = Get-Content $libJson -Raw | ConvertFrom-Json
$densityData = Get-Content $densityJson -Raw | ConvertFrom-Json

# ── Generate combined HTML ──
Write-Host "`nGenerating report..." -ForegroundColor Yellow

$hostname = $env:COMPUTERNAME
$timestamp = Get-Date -Format "yyyy-MM-dd HH:mm"

# Serialize for JS
$cliJs = $cliData | ConvertTo-Json -Depth 10 -Compress
$libJs = $libData | ConvertTo-Json -Depth 10 -Compress
$densityJs = $densityData | ConvertTo-Json -Depth 10 -Compress

$html = @"
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>MXC Benchmark Report</title>
<style>
  :root {
    --bg: #F6F7F9; --surface: #FFF; --text: #1A1D23; --text2: #5A6172;
    --border: #D8DAE0; --grid: #E8EAEF;
    --hl: #3B6CE7; --nv: #1B9E6D; --wl: #C05621;
    --good: #059669; --warn: #d97706; --bad: #dc2626;
    --mono: ui-monospace, "Cascadia Code", "SF Mono", Menlo, monospace;
    --sans: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #13141A; --surface: #1C1D25; --text: #E4E5EA; --text2: #8B90A0;
      --border: #2E3038; --grid: #252730;
      --hl: #6B9BF7; --nv: #3FB950; --wl: #E8854A;
      --good: #3fb950; --warn: #e8854a; --bad: #f85149;
    }
  }
  :root[data-theme="dark"] {
    --bg: #13141A; --surface: #1C1D25; --text: #E4E5EA; --text2: #8B90A0;
    --border: #2E3038; --grid: #252730;
    --hl: #6B9BF7; --nv: #3FB950; --wl: #E8854A;
    --good: #3fb950; --warn: #e8854a; --bad: #f85149;
  }
  :root[data-theme="light"] {
    --bg: #F6F7F9; --surface: #FFF; --text: #1A1D23; --text2: #5A6172;
    --border: #D8DAE0; --grid: #E8EAEF;
    --hl: #3B6CE7; --nv: #1B9E6D; --wl: #C05621;
    --good: #059669; --warn: #d97706; --bad: #dc2626;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { background: var(--bg); color: var(--text); font-family: var(--sans); line-height: 1.6; padding: 2.5rem 1rem 3rem; }
  .c { max-width: 920px; margin: 0 auto; }
  .eyebrow { font-size: .7rem; font-weight: 600; letter-spacing: .1em; text-transform: uppercase; color: var(--text2); margin-bottom: .25rem; }
  h1 { font-size: 1.5rem; font-weight: 700; margin-bottom: .3rem; }
  .sub { font-size: .85rem; color: var(--text2); margin-bottom: 2rem; }

  .section-head { margin: 2.5rem 0 1rem; padding-bottom: .5rem; border-bottom: 2px solid var(--border); }
  .section-head h2 { font-size: 1.1rem; font-weight: 700; }
  .section-head p { font-size: .8rem; color: var(--text2); margin-top: .15rem; }

  .target { display: inline-block; font-size: .7rem; font-weight: 600; padding: .15em .5em; border-radius: 3px; margin-left: .5rem; vertical-align: middle; }
  .target-pass { background: var(--good); color: #fff; }
  .target-warn { background: var(--warn); color: #fff; }
  .target-fail { background: var(--bad); color: #fff; }

  .cards { display: flex; gap: 1rem; flex-wrap: wrap; margin-bottom: 1.5rem; }
  .card { flex: 1; min-width: 160px; background: var(--surface); border: 1px solid var(--border); border-radius: 6px; padding: .85rem 1rem; }
  .card-label { font-size: .68rem; font-weight: 600; letter-spacing: .06em; text-transform: uppercase; color: var(--text2); }
  .card-val { font-family: var(--mono); font-size: 1.5rem; font-weight: 700; font-variant-numeric: tabular-nums; line-height: 1.3; }
  .card-note { font-size: .72rem; color: var(--text2); }

  .panel { background: var(--surface); border: 1px solid var(--border); border-radius: 6px; padding: 1.25rem; margin-bottom: 1.5rem; }
  .stitle { font-size: .78rem; font-weight: 600; color: var(--text2); margin-bottom: .75rem; }
  canvas { display: block; width: 100%; height: auto; }

  .tbl-panel { background: var(--surface); border: 1px solid var(--border); border-radius: 6px; overflow: hidden; margin-bottom: 1.5rem; }
  .tbl-head { padding: .85rem 1rem .5rem; font-size: .78rem; font-weight: 600; color: var(--text2); }
  .tbl-wrap { overflow-x: auto; }
  table { width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; }
  th { font-size: .68rem; font-weight: 600; letter-spacing: .04em; text-transform: uppercase; color: var(--text2); text-align: right; padding: .45rem .75rem; border-bottom: 1px solid var(--border); white-space: nowrap; }
  th:first-child { text-align: left; }
  td { font-family: var(--mono); font-size: .8rem; text-align: right; padding: .55rem .75rem; border-bottom: 1px solid var(--grid); white-space: nowrap; }
  td:first-child { font-family: var(--sans); font-weight: 600; text-align: left; }
  tr:last-child td { border-bottom: none; }
  .note { font-size: .78rem; color: var(--text2); margin-bottom: 1rem; line-height: 1.6; }
  .note code { font-family: var(--mono); font-size: .75rem; background: var(--bg); padding: .1em .35em; border-radius: 3px; }
  footer { text-align: center; font-size: .7rem; color: var(--text2); padding-top: 1.5rem; }
</style>
</head>
<body>
<div class="c">
  <div class="eyebrow">MXC Containment Benchmark</div>
  <h1>hello workload &mdash; full report</h1>
  <div class="sub">$hostname &middot; $timestamp &middot; $Iterations iterations per mode</div>

  <!-- targets from Stuart's doc section 4.2 -->
  <div class="panel">
    <div class="stitle">Targets (micro-VM architecture doc &sect;4.2)</div>
    <table>
      <thead><tr><th>Metric</th><th>Target</th><th>Source</th></tr></thead>
      <tbody>
        <tr><td>Cold-start p99</td><td>&le; 100 ms</td><td style="font-family:var(--sans);text-align:left">&sect;4.2 startup budget</td></tr>
        <tr><td>Warm activation</td><td>&le; 10 ms</td><td style="font-family:var(--sans);text-align:left">&sect;4.2 forked/snapshot</td></tr>
        <tr><td>Per-sandbox WS</td><td>&le; 128 MB (cap 256)</td><td style="font-family:var(--sans);text-align:left">&sect;4.2 stress envelope</td></tr>
        <tr><td>Density</td><td>&ge; 12 in 1.5 GB</td><td style="font-family:var(--sans);text-align:left">&sect;4.2 8 GB laptop budget</td></tr>
      </tbody>
    </table>
  </div>

  <div class="section-head" id="cold"><h2>Cold-Start Latency</h2><p>Full wxc-exec process per iteration (CLI mode)</p></div>
  <div class="cards" id="cold-cards"></div>
  <div class="panel"><div class="stitle">Per-iteration trace</div><canvas id="cold-chart" width="1600" height="360"></canvas></div>
  <div class="tbl-panel"><div class="tbl-head">Statistics (ms)</div><div class="tbl-wrap"><table id="cold-tbl"></table></div></div>

  <div class="section-head" id="warm"><h2>Warm-Start Latency</h2><p>In-process runner reuse (library mode)</p></div>
  <div class="cards" id="warm-cards"></div>
  <div class="panel"><div class="stitle">Per-iteration trace</div><canvas id="warm-chart" width="1600" height="360"></canvas></div>
  <div class="tbl-panel"><div class="tbl-head">Statistics (ms)</div><div class="tbl-wrap"><table id="warm-tbl"></table></div></div>

  <div class="section-head" id="mem"><h2>Per-VM Memory</h2><p>Peak working set during wxc-exec invocation</p></div>
  <div class="cards" id="mem-cards"></div>

  <div class="section-head" id="density"><h2>Density</h2><p>Concurrent runners &mdash; process working set growth</p></div>
  <div class="cards" id="density-cards"></div>
  <div class="tbl-panel"><div class="tbl-head">Per-runner working set growth</div><div class="tbl-wrap"><table id="density-tbl"></table></div></div>

  <div class="section-head" id="disk"><h2>Disk Footprint</h2><p>On-disk sizes (sparse-aware for NTFS)</p></div>
  <div class="tbl-panel"><div class="tbl-head">Component sizes</div><div class="tbl-wrap"><table id="disk-tbl"></table></div></div>

  <footer>wxc-exec release build &middot; WHP snapshots &middot; hello workload</footer>
</div>

<script>
const CLI_RAW = $cliJs;
const LIB_RAW = $libJs;
const DENSITY_RAW = $densityJs;

const BACKEND_COLORS = { hyperlight: 'var(--hl)', microvm: 'var(--nv)', wslc: 'var(--wl)' };
const BACKEND_LABELS = { hyperlight: 'Hyperlight', microvm: 'NanVix', wslc: 'WSLc' };

function css(p) { return getComputedStyle(document.documentElement).getPropertyValue(p).trim(); }
function rc(c) { return c.startsWith('var(') ? css(c.slice(4,-1)) : c; }

// ── Parse CLI data ──
// CLI data comes from bench_containments.ps1 JSON: { results: { backend: { ... } }, ... }
const CLI = {};
if (CLI_RAW && CLI_RAW.results) {
  for (const [k, v] of Object.entries(CLI_RAW.results)) {
    CLI[k] = {
      label: BACKEND_LABELS[k] || k,
      color: BACKEND_COLORS[k] || 'var(--text)',
      raw: v.rawTimingsMs || [],
      min: v.timingMs ? v.timingMs.Min : 0,
      median: v.timingMs ? v.timingMs.Median : 0,
      mean: v.timingMs ? v.timingMs.Mean : 0,
      p95: v.timingMs ? (v.timingMs.P90 || v.timingMs.P99) : 0,
      max: v.timingMs ? v.timingMs.Max : 0,
      stdev: v.timingMs ? v.timingMs.StdDev : 0,
      memory: v.memoryMB || null,
    };
  }
}

// ── Parse library data ──
// Library data is an array of BackendResult objects
const LIB = {};
const libArr = Array.isArray(LIB_RAW) ? LIB_RAW : [];
const backendKeyMap = { 'Hyperlight': 'hyperlight', 'NanVix (MicroVM)': 'microvm', 'WSLc': 'wslc' };
libArr.forEach(d => {
  if (d.error) return;
  const k = backendKeyMap[d.backend] || d.backend.toLowerCase();
  LIB[k] = {
    label: d.backend,
    color: BACKEND_COLORS[k] || 'var(--text)',
    raw: d.iterations.map(it => it.elapsed_ms),
    min: d.stats.min_ms,
    median: d.stats.median_ms,
    mean: d.stats.mean_ms,
    p95: d.stats.p95_ms,
    max: d.stats.max_ms,
    stdev: d.stats.stdev_ms,
    setup: d.runner_create_ms || 0,
  };
});

// ── Parse density data ──
const DENSITY = {};
const densArr = Array.isArray(DENSITY_RAW) ? DENSITY_RAW : [];
densArr.forEach(d => {
  const k = backendKeyMap[d.backend] || d.backend.toLowerCase();
  DENSITY[k] = d;
});

// ── Render helpers ──
function makeCards(elId, data, unit, note) {
  const el = document.getElementById(elId);
  if (!el) return;
  Object.entries(data).forEach(([k, d]) => {
    const c = rc(d.color);
    const card = document.createElement('div'); card.className = 'card';
    card.innerHTML = '<div class="card-label">' + d.label + '</div>'
      + '<div class="card-val" style="color:'+c+'">' + (typeof d.median === 'number' ? d.median.toFixed(1) : d.median) + '<span style="font-size:.7rem;opacity:.7"> '+unit+'</span></div>'
      + '<div class="card-note">' + note + '</div>';
    el.appendChild(card);
  });
}

function makeTable(elId, data, cols) {
  const tbl = document.getElementById(elId);
  if (!tbl) return;
  let h = '<thead><tr>' + cols.map(c => '<th>'+c.label+'</th>').join('') + '</tr></thead><tbody>';
  Object.entries(data).forEach(([k, d]) => {
    const c = rc(d.color);
    h += '<tr>';
    cols.forEach((col, i) => {
      const val = col.get(d);
      const style = i === 0 ? ' style="color:'+c+'"' : '';
      h += '<td'+style+'>' + val + '</td>';
    });
    h += '</tr>';
  });
  h += '</tbody>';
  tbl.innerHTML = h;
}

function setupCanvas(id) {
  const c = document.getElementById(id);
  if (!c) return null;
  const dpr = window.devicePixelRatio || 1;
  const logW = c.width / 2, logH = c.height / 2;
  c.style.width = logW + 'px'; c.style.height = logH + 'px';
  c.width = logW * dpr; c.height = logH * dpr;
  const ctx = c.getContext('2d'); ctx.scale(dpr, dpr);
  return { ctx, W: logW, H: logH };
}

function drawLineChart(canvasId, data) {
  const r = setupCanvas(canvasId);
  if (!r) return;
  const { ctx, W, H } = r;
  const pad = { top: 20, right: 30, bottom: 48, left: 60 };
  const plotW = W - pad.left - pad.right, plotH = H - pad.top - pad.bottom;
  const entries = Object.values(data);
  if (!entries.length) return;
  const allVals = entries.flatMap(d => d.raw);
  if (!allVals.length) return;
  const yMin = Math.floor(Math.min(...allVals) / 50) * 50 - 20;
  const yMax = Math.ceil(Math.max(...allVals) / 50) * 50 + 20;
  const n = Math.max(...entries.map(d => d.raw.length));
  const xPos = i => pad.left + (i / Math.max(n - 1, 1)) * plotW;
  const yPos = v => pad.top + plotH - ((v - yMin) / (yMax - yMin)) * plotH;

  const gridColor = css('--grid'), text2 = css('--text2');
  ctx.strokeStyle = gridColor; ctx.lineWidth = 1;
  const yRange = yMax - yMin;
  const yStep = yRange > 2000 ? 500 : yRange > 500 ? 200 : yRange > 100 ? 50 : 10;
  ctx.font = '11px ' + css('--mono'); ctx.fillStyle = text2;
  ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
  for (let v = Math.ceil(yMin/yStep)*yStep; v <= yMax; v += yStep) {
    const y = Math.round(yPos(v)) + 0.5;
    ctx.beginPath(); ctx.moveTo(pad.left, y); ctx.lineTo(W - pad.right, y); ctx.stroke();
    ctx.fillText(v+'', pad.left - 7, y);
  }
  ctx.textAlign = 'center'; ctx.textBaseline = 'top';
  for (let i = 0; i < n; i++) ctx.fillText('#'+(i+1), xPos(i), H - pad.bottom + 7);
  ctx.font = '11px ' + css('--sans'); ctx.fillText('Iteration', pad.left + plotW/2, H - pad.bottom + 24);

  entries.forEach((d, di) => {
    const color = rc(d.color);
    ctx.beginPath(); ctx.moveTo(xPos(0), yPos(yMin));
    d.raw.forEach((v, i) => ctx.lineTo(xPos(i), yPos(v)));
    ctx.lineTo(xPos(d.raw.length - 1), yPos(yMin)); ctx.closePath();
    ctx.fillStyle = color + '18'; ctx.fill();
    ctx.beginPath(); d.raw.forEach((v, i) => { if (i === 0) ctx.moveTo(xPos(0), yPos(v)); else ctx.lineTo(xPos(i), yPos(v)); });
    ctx.strokeStyle = color; ctx.lineWidth = 2.5; ctx.lineJoin = 'round'; ctx.stroke();
    d.raw.forEach((v, i) => {
      ctx.beginPath(); ctx.arc(xPos(i), yPos(v), 3.5, 0, Math.PI*2);
      ctx.fillStyle = css('--surface'); ctx.fill();
      ctx.strokeStyle = color; ctx.lineWidth = 2; ctx.stroke();
    });
    // inline legend
    const lx = pad.left + 10 + di * 140;
    ctx.fillStyle = color; ctx.fillRect(lx, pad.top + 6, 10, 10);
    ctx.fillStyle = css('--text'); ctx.textAlign = 'left'; ctx.font = '11px ' + css('--sans');
    ctx.fillText(d.label, lx + 14, pad.top + 14);
  });
}

// ── Cold start ──
makeCards('cold-cards', CLI, 'ms', 'median cold-start');
drawLineChart('cold-chart', CLI);
makeTable('cold-tbl', CLI, [
  { label: 'Backend', get: d => d.label },
  { label: 'Min', get: d => d.min.toFixed(1) },
  { label: 'Median', get: d => d.median.toFixed(1) },
  { label: 'Mean', get: d => d.mean.toFixed(1) },
  { label: 'P95', get: d => d.p95.toFixed(1) },
  { label: 'Max', get: d => d.max.toFixed(1) },
  { label: 'StdDev', get: d => d.stdev.toFixed(1) },
]);

// ── Warm start ──
makeCards('warm-cards', LIB, 'ms', 'median warm-start');
drawLineChart('warm-chart', LIB);
makeTable('warm-tbl', LIB, [
  { label: 'Backend', get: d => d.label },
  { label: 'Min', get: d => d.min.toFixed(1) },
  { label: 'Median', get: d => d.median.toFixed(1) },
  { label: 'Mean', get: d => d.mean.toFixed(1) },
  { label: 'P95', get: d => d.p95.toFixed(1) },
  { label: 'Max', get: d => d.max.toFixed(1) },
  { label: 'StdDev', get: d => d.stdev.toFixed(1) },
  { label: 'Setup', get: d => (d.setup||0).toFixed(0) + 'ms' },
]);

// ── Memory ──
const MEM = {};
Object.entries(CLI).forEach(([k, d]) => {
  if (d.memory) {
    MEM[k] = { label: d.label, color: d.color, median: d.memory.Median || d.memory.median || 0 };
  }
});
makeCards('mem-cards', MEM, 'MB', 'peak working set');

// ── Density ──
const DENS_CARDS = {};
Object.entries(DENSITY).forEach(([k, d]) => {
  DENS_CARDS[k] = {
    label: BACKEND_LABELS[k] || k,
    color: BACKEND_COLORS[k] || 'var(--text)',
    median: d.per_runner_mb ? d.per_runner_mb.toFixed(1) : '?',
  };
});
makeCards('density-cards', DENS_CARDS, 'MB/runner', 'working set per runner');

// Density table
(function() {
  const tbl = document.getElementById('density-tbl');
  if (!tbl) return;
  let h = '<thead><tr><th>Backend</th><th>Runners</th><th>Baseline (MB)</th><th>Final (MB)</th><th>Per-Runner (MB)</th><th>Fit in 1.5 GB</th></tr></thead><tbody>';
  Object.entries(DENSITY).forEach(([k, d]) => {
    const c = rc(BACKEND_COLORS[k] || 'var(--text)');
    const fits = d.per_runner_mb > 0 ? Math.floor((1500 - d.baseline_ws_mb) / d.per_runner_mb) : '?';
    h += '<tr><td style="color:'+c+'">'+(BACKEND_LABELS[k]||k)+'</td>';
    h += '<td>'+d.count+'</td><td>'+d.baseline_ws_mb.toFixed(1)+'</td><td>'+d.final_ws_mb.toFixed(1)+'</td>';
    h += '<td>'+d.per_runner_mb.toFixed(1)+'</td><td>~'+fits+'</td></tr>';
  });
  h += '</tbody>';
  tbl.innerHTML = h;
})();

// ── Disk footprint ──
(function() {
  const tbl = document.getElementById('disk-tbl');
  if (!tbl) return;
  const fp = CLI_RAW && CLI_RAW.footprint ? CLI_RAW.footprint : null;
  if (!fp) { tbl.innerHTML = '<tbody><tr><td>No footprint data</td></tr></tbody>'; return; }
  let h = '<thead><tr><th>Component</th><th>Size (MB)</th><th>Detail</th></tr></thead><tbody>';
  if (fp.hyperlight) h += '<tr><td>Hyperlight snapshot</td><td>'+(fp.hyperlight.actualMB||fp.hyperlight.sizeMB||'?')+'</td><td style="font-family:var(--sans);text-align:left">'+
    (fp.hyperlight.actualMB ? 'actual on disk ('+fp.hyperlight.logicalMB+' MB logical, sparse)' : 'total')+'</td></tr>';
  if (fp.nanvix) h += '<tr><td>NanVix files</td><td>'+(fp.nanvix.sizeMB||'?')+'</td><td style="font-family:var(--sans);text-align:left">rootfs + initrd + kernel + daemon</td></tr>';
  if (fp.wslc) h += '<tr><td>WSLc image cache</td><td>'+(fp.wslc.sizeMB||'?')+'</td><td style="font-family:var(--sans);text-align:left">'+(fp.wslc.files||'?')+' files</td></tr>';
  h += '</tbody>';
  tbl.innerHTML = h;
})();
</script>
</body>
</html>
"@

Set-Content -Path $Output -Value $html -Encoding UTF8
Write-Host "`n====================================" -ForegroundColor Green
Write-Host "  Report written to: $Output" -ForegroundColor Green
Write-Host "====================================" -ForegroundColor Green
Write-Host "  Temp data: $tempDir"

# Clean up
Remove-Item $tempDir -Recurse -Force -ErrorAction SilentlyContinue
