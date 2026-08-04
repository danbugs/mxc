<#
.SYNOPSIS
    Benchmark harness for MXC containment backends.

.DESCRIPTION
    Runs a workload across multiple containment backends (hyperlight, microvm/nanvix, wslc)
    and collects wall-clock timing, exit codes, and peak memory usage per iteration.
    Outputs a summary table and optionally a JSON results file.

.PARAMETER Backends
    Which backends to benchmark. Comma-separated list from: hyperlight, microvm, wslc.
    Default: all available backends.

.PARAMETER Workload
    Workload name (maps to tests/bench/workloads/{name}_{backend}.json).
    Default: hello.

.PARAMETER Iterations
    Number of iterations per backend. Default: 10.

.PARAMETER WarmupIterations
    Number of warmup iterations (not counted in stats). Default: 1.

.PARAMETER OutputJson
    Path to write JSON results. Optional.

.PARAMETER SkipSetup
    Skip backend setup (assume already set up). Default: false.

.PARAMETER WxcExe
    Path to wxc-exec.exe. Auto-discovered if not specified.

.PARAMETER WslcImage
    Container image for wslc backend. Default: python:3.12-alpine.

.EXAMPLE
    .\bench_containments.ps1 -Backends hyperlight,microvm -Iterations 20

.EXAMPLE
    .\bench_containments.ps1 -Workload stdlib -OutputJson results.json
#>

[CmdletBinding()]
param(
    [string]$Backends = "",
    [string]$Workload = "hello",
    [int]$Iterations = 10,
    [int]$WarmupIterations = 1,
    [string]$OutputJson = "",
    [switch]$SkipSetup,
    [string]$WxcExe = "",
    [string]$WslcImage = "python:3.12-alpine"
)

$ErrorActionPreference = "Stop"

# --- Locate wxc-exec.exe ---

function Find-WxcExe {
    if ($WxcExe -ne "" -and (Test-Path $WxcExe)) {
        return $WxcExe
    }

    $scriptDir = Split-Path -Parent $PSScriptRoot
    $srcDir = Join-Path $scriptDir "src"

    $profiles = @("release", "debug")
    $triples = @("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")

    $candidates = @()
    foreach ($profile in $profiles) {
        foreach ($triple in $triples) {
            $candidates += Join-Path $srcDir "target" $triple $profile "wxc-exec.exe"
        }
        $candidates += Join-Path $srcDir "target" $profile "wxc-exec.exe"
    }

    $found = $candidates | Where-Object { Test-Path $_ } |
        Sort-Object { (Get-Item $_).LastWriteTime } -Descending |
        Select-Object -First 1

    if (-not $found) {
        Write-Error "wxc-exec.exe not found. Build first: cargo build --release -p wxc --features hyperlight,microvm,wslc"
        exit 1
    }

    return $found
}

# --- Backend availability checks ---

function Test-HyperlightAvailable {
    param([string]$ExeDir)
    $snapshot = Join-Path $env:LOCALAPPDATA "pyhl" "snapshot" "index.json"
    return (Test-Path $snapshot)
}

function Test-MicrovmAvailable {
    param([string]$ExeDir)
    $required = @("nanvixd.exe", "nanvix_rootfs.img", "python3.initrd")
    $allPresent = $true
    foreach ($f in $required) {
        if (-not (Test-Path (Join-Path $ExeDir $f))) {
            $allPresent = $false
            break
        }
    }
    if ($allPresent) {
        $allPresent = Test-Path (Join-Path $ExeDir "bin" "kernel.elf")
    }
    return $allPresent
}

function Test-WslcAvailable {
    # wslc requires the WSLC SDK DLLs and a pulled image.
    # Best-effort: check if wxc-exec was built with wslc feature by trying --probe.
    return $true  # We'll catch failures at runtime
}

# --- Setup backends ---

function Invoke-Setup {
    param(
        [string]$Exe,
        [string]$Backend
    )

    Write-Host "`n=== Setting up $Backend ===" -ForegroundColor Cyan

    switch ($Backend) {
        "hyperlight" {
            Write-Host "Running: wxc-exec --setup-hyperlight"
            $p = Start-Process -FilePath $Exe -ArgumentList "--setup-hyperlight" `
                -NoNewWindow -Wait -PassThru -RedirectStandardError "$env:TEMP\bench_setup_hl.log"
            if ($p.ExitCode -ne 0) {
                Write-Warning "Hyperlight setup failed (exit $($p.ExitCode)). Check $env:TEMP\bench_setup_hl.log"
                return $false
            }
            Write-Host "Hyperlight setup complete." -ForegroundColor Green
            return $true
        }
        "microvm" {
            # Nanvix binaries must be next to wxc-exec.exe (built via cargo).
            # Snapshot is auto-generated on first run.
            $exeDir = Split-Path -Parent $Exe
            if (-not (Test-MicrovmAvailable $exeDir)) {
                Write-Warning "NanVix binaries not found next to wxc-exec.exe. Build with --features microvm."
                return $false
            }
            Write-Host "NanVix binaries present. Snapshot will auto-generate on first run." -ForegroundColor Green
            return $true
        }
        "wslc" {
            Write-Host "Running: wxc-exec --setup-wslc --image $WslcImage"
            $p = Start-Process -FilePath $Exe `
                -ArgumentList "--setup-wslc","--image",$WslcImage `
                -NoNewWindow -Wait -PassThru -RedirectStandardError "$env:TEMP\bench_setup_wslc.log"
            if ($p.ExitCode -ne 0) {
                Write-Warning "WSLc setup failed (exit $($p.ExitCode)). Check $env:TEMP\bench_setup_wslc.log"
                return $false
            }
            Write-Host "WSLc setup complete." -ForegroundColor Green
            return $true
        }
    }
    return $false
}

# --- Run a single iteration ---

function Invoke-Iteration {
    param(
        [string]$Exe,
        [string]$ConfigPath,
        [string]$Backend
    )

    $extraArgs = @("--experimental", "--debug")

    $sw = [System.Diagnostics.Stopwatch]::StartNew()

    $proc = Start-Process -FilePath $Exe `
        -ArgumentList (@($ConfigPath) + $extraArgs) `
        -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput "$env:TEMP\bench_stdout.txt" `
        -RedirectStandardError "$env:TEMP\bench_stderr.txt"

    $sw.Stop()

    $stdout = ""
    if (Test-Path "$env:TEMP\bench_stdout.txt") {
        $stdout = Get-Content "$env:TEMP\bench_stdout.txt" -Raw -ErrorAction SilentlyContinue
    }
    $stderr = ""
    if (Test-Path "$env:TEMP\bench_stderr.txt") {
        $stderr = Get-Content "$env:TEMP\bench_stderr.txt" -Raw -ErrorAction SilentlyContinue
    }

    # Try to extract peak working set from the process (best-effort).
    $peakWsMb = -1
    try {
        if ($null -ne $proc -and $null -ne $proc.PeakWorkingSet64) {
            $peakWsMb = [math]::Round($proc.PeakWorkingSet64 / 1MB, 2)
        }
    } catch {}

    return @{
        ExitCode     = $proc.ExitCode
        ElapsedMs    = $sw.Elapsed.TotalMilliseconds
        PeakWsMb     = $peakWsMb
        Stdout       = $stdout
        Stderr       = $stderr
    }
}

# --- Statistics ---

function Get-Stats {
    param([double[]]$Values)

    if ($Values.Count -eq 0) {
        return @{ Min = 0; Max = 0; Mean = 0; Median = 0; P90 = 0; P99 = 0; StdDev = 0 }
    }

    $sorted = $Values | Sort-Object
    $count = $sorted.Count
    $sum = ($sorted | Measure-Object -Sum).Sum
    $mean = $sum / $count

    $median = if ($count % 2 -eq 0) {
        ($sorted[$count/2 - 1] + $sorted[$count/2]) / 2
    } else {
        $sorted[[math]::Floor($count/2)]
    }

    $p90idx = [math]::Min([math]::Ceiling($count * 0.90) - 1, $count - 1)
    $p99idx = [math]::Min([math]::Ceiling($count * 0.99) - 1, $count - 1)

    $variance = ($sorted | ForEach-Object { ($_ - $mean) * ($_ - $mean) } |
                 Measure-Object -Sum).Sum / $count
    $stddev = [math]::Sqrt($variance)

    return @{
        Min    = [math]::Round($sorted[0], 2)
        Max    = [math]::Round($sorted[-1], 2)
        Mean   = [math]::Round($mean, 2)
        Median = [math]::Round($median, 2)
        P90    = [math]::Round($sorted[$p90idx], 2)
        P99    = [math]::Round($sorted[$p99idx], 2)
        StdDev = [math]::Round($stddev, 2)
    }
}

# --- Main ---

$wxcExe = Find-WxcExe
$exeDir = Split-Path -Parent $wxcExe
Write-Host "Using wxc-exec: $wxcExe" -ForegroundColor Cyan

# Resolve backends
$allBackends = @("hyperlight", "microvm", "wslc")
if ($Backends -ne "") {
    $selectedBackends = $Backends -split "," | ForEach-Object { $_.Trim().ToLower() }
} else {
    $selectedBackends = @()
    if (Test-HyperlightAvailable $exeDir) { $selectedBackends += "hyperlight" }
    if (Test-MicrovmAvailable $exeDir) { $selectedBackends += "microvm" }
    $selectedBackends += "wslc"  # Always try wslc; will fail gracefully
}

Write-Host "Backends: $($selectedBackends -join ', ')" -ForegroundColor Cyan
Write-Host "Workload: $Workload" -ForegroundColor Cyan
Write-Host "Iterations: $Iterations (+ $WarmupIterations warmup)" -ForegroundColor Cyan

# Resolve workload config directory
$repoRoot = Split-Path -Parent $PSScriptRoot
$workloadDir = Join-Path $repoRoot "tests" "bench" "workloads"
if (-not (Test-Path $workloadDir)) {
    Write-Error "Workload directory not found: $workloadDir"
    exit 1
}

# Setup phase
if (-not $SkipSetup) {
    foreach ($backend in $selectedBackends) {
        $ok = Invoke-Setup -Exe $wxcExe -Backend $backend
        if (-not $ok) {
            Write-Warning "Skipping $backend (setup failed)"
            $selectedBackends = $selectedBackends | Where-Object { $_ -ne $backend }
        }
    }
}

if ($selectedBackends.Count -eq 0) {
    Write-Error "No backends available to benchmark."
    exit 1
}

# Benchmark phase
$allResults = @{}

foreach ($backend in $selectedBackends) {
    $configFile = Join-Path $workloadDir "${Workload}_${backend}.json"
    if (-not (Test-Path $configFile)) {
        Write-Warning "Config not found for $backend workload '$Workload': $configFile — skipping"
        continue
    }

    Write-Host "`n=== Benchmarking: $backend ($Workload) ===" -ForegroundColor Yellow

    $timings = @()
    $peakMemory = @()
    $failures = 0
    $totalRuns = $WarmupIterations + $Iterations

    for ($i = 1; $i -le $totalRuns; $i++) {
        $isWarmup = $i -le $WarmupIterations
        $label = if ($isWarmup) { "warmup $i/$WarmupIterations" } else { "iter $($i - $WarmupIterations)/$Iterations" }

        Write-Host "  [$backend] $label ... " -NoNewline

        $result = Invoke-Iteration -Exe $wxcExe -ConfigPath $configFile -Backend $backend

        if ($result.ExitCode -eq 0) {
            $color = "Green"
            $status = "OK"
        } else {
            $color = "Red"
            $status = "FAIL(exit=$($result.ExitCode))"
            if (-not $isWarmup) { $failures++ }
        }

        $memStr = if ($result.PeakWsMb -ge 0) { "$($result.PeakWsMb) MB" } else { "n/a" }
        Write-Host "$([math]::Round($result.ElapsedMs, 1)) ms | mem=$memStr | $status" -ForegroundColor $color

        if (-not $isWarmup) {
            $timings += $result.ElapsedMs
            if ($result.PeakWsMb -ge 0) {
                $peakMemory += $result.PeakWsMb
            }
        }
    }

    $stats = Get-Stats $timings
    $memStats = if ($peakMemory.Count -gt 0) { Get-Stats $peakMemory } else { $null }

    $allResults[$backend] = @{
        Backend      = $backend
        Workload     = $Workload
        Iterations   = $Iterations
        Failures     = $failures
        TimingMs     = $stats
        PeakMemoryMb = $memStats
        RawTimings   = $timings
    }
}

# --- Summary ---

Write-Host "`n" -NoNewline
Write-Host ("=" * 80) -ForegroundColor Cyan
Write-Host "  BENCHMARK RESULTS: $Workload workload ($Iterations iterations)" -ForegroundColor Cyan
Write-Host ("=" * 80) -ForegroundColor Cyan

$header = "{0,-15} {1,10} {2,10} {3,10} {4,10} {5,10} {6,10} {7,10}" -f `
    "Backend", "Min(ms)", "Median", "Mean", "P90", "P99", "Max(ms)", "Fails"
Write-Host $header -ForegroundColor White
Write-Host ("-" * 80)

foreach ($backend in $selectedBackends) {
    if (-not $allResults.ContainsKey($backend)) { continue }
    $r = $allResults[$backend]
    $t = $r.TimingMs

    $row = "{0,-15} {1,10} {2,10} {3,10} {4,10} {5,10} {6,10} {7,10}" -f `
        $backend, $t.Min, $t.Median, $t.Mean, $t.P90, $t.P99, $t.Max, $r.Failures
    Write-Host $row
}

if ($allResults.Values | Where-Object { $_.PeakMemoryMb }) {
    Write-Host ""
    $memHeader = "{0,-15} {1,12} {2,12} {3,12} {4,12}" -f `
        "Backend", "MinMem(MB)", "MedMem", "MeanMem", "MaxMem"
    Write-Host $memHeader -ForegroundColor White
    Write-Host ("-" * 65)
    foreach ($backend in $selectedBackends) {
        if (-not $allResults.ContainsKey($backend)) { continue }
        $m = $allResults[$backend].PeakMemoryMb
        if ($m) {
            $row = "{0,-15} {1,12} {2,12} {3,12} {4,12}" -f `
                $backend, $m.Min, $m.Median, $m.Mean, $m.Max
            Write-Host $row
        }
    }
}

Write-Host ""

# --- JSON output ---

if ($OutputJson -ne "") {
    $jsonObj = @{
        timestamp  = (Get-Date -Format "o")
        workload   = $Workload
        iterations = $Iterations
        wxcExe     = $wxcExe
        hostname   = $env:COMPUTERNAME
        results    = @{}
    }
    foreach ($k in $allResults.Keys) {
        $r = $allResults[$k]
        $jsonObj.results[$k] = @{
            backend      = $r.Backend
            workload     = $r.Workload
            iterations   = $r.Iterations
            failures     = $r.Failures
            timingMs     = $r.TimingMs
            peakMemoryMb = $r.PeakMemoryMb
            rawTimingsMs = $r.RawTimings
        }
    }
    $jsonObj | ConvertTo-Json -Depth 5 | Set-Content -Path $OutputJson -Encoding UTF8
    Write-Host "Results written to: $OutputJson" -ForegroundColor Green
}
