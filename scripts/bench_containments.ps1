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

.PARAMETER OutputHtml
    Path to write an HTML report with charts. Optional.

.EXAMPLE
    .\bench_containments.ps1 -Workload stdlib -OutputJson results.json

.EXAMPLE
    .\bench_containments.ps1 -Backends hyperlight,microvm,wslc -OutputHtml report.html
#>

[CmdletBinding()]
param(
    [string]$Backends = "",
    [string]$Workload = "hello",
    [int]$Iterations = 10,
    [int]$WarmupIterations = 1,
    [string]$OutputJson = "",
    [string]$OutputHtml = "",
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
            $candidates += Join-Path (Join-Path (Join-Path (Join-Path $srcDir "target") $triple) $profile) "wxc-exec.exe"
        }
        $candidates += Join-Path (Join-Path (Join-Path $srcDir "target") $profile) "wxc-exec.exe"
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
    $snapshot = Join-Path (Join-Path (Join-Path $env:LOCALAPPDATA "pyhl") "snapshot") "index.json"
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
        $allPresent = Test-Path (Join-Path (Join-Path $ExeDir "bin") "kernel.elf")
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

    # Use raw .NET Process API to avoid Start-Process console-allocation overhead (~250ms).
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = "$ConfigPath --experimental --debug"
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    $p = New-Object System.Diagnostics.Process
    $p.StartInfo = $psi

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    [void]$p.Start()

    # Read stdout/stderr async to avoid deadlocks when buffers fill
    $stdoutTask = $p.StandardOutput.ReadToEndAsync()
    $stderrTask = $p.StandardError.ReadToEndAsync()

    # Poll for peak working set while process runs
    $peakWsMB = 0
    while (-not $p.HasExited) {
        try {
            $p.Refresh()
            $wsMB = [math]::Round($p.PeakWorkingSet64 / 1MB, 2)
            if ($wsMB -gt $peakWsMB) { $peakWsMB = $wsMB }
        } catch {}
        Start-Sleep -Milliseconds 5
    }
    # Final read after exit (handle still open)
    try {
        $p.Refresh()
        $wsMB = [math]::Round($p.PeakWorkingSet64 / 1MB, 2)
        if ($wsMB -gt $peakWsMB) { $peakWsMB = $wsMB }
    } catch {}

    $sw.Stop()

    $stdoutText = $stdoutTask.GetAwaiter().GetResult()
    $stderrText = $stderrTask.GetAwaiter().GetResult()
    $exitCode = $p.ExitCode
    $p.Dispose()

    $combined = "$stdoutText$stderrText"

    # Parse restore/call timing from hyperlight/nanvix log lines if present
    $restoreMs = -1
    $callMs = -1
    $runnerMs = -1
    if ($combined -match 'restore=([0-9.]+)ms\s+call=([0-9.]+)ms') {
        $restoreMs = [double]$Matches[1]
        $callMs = [double]$Matches[2]
    }
    if ($combined -match 'Runner completed in (\d+)ms') {
        $runnerMs = [double]$Matches[1]
    }

    return @{
        ExitCode   = $exitCode
        ElapsedMs  = $sw.Elapsed.TotalMilliseconds
        RunnerMs   = $runnerMs
        RestoreMs  = $restoreMs
        CallMs     = $callMs
        PeakMemMB  = $peakWsMB
        Output     = $combined
    }
}

# --- Memory/disk measurement ---

function Measure-BackendFootprint {
    param([string]$ExeDir)

    $footprint = @{}

    # Hyperlight snapshot size (actual on-disk allocation, not sparse/apparent)
    $snapshotDir = Join-Path (Join-Path $env:LOCALAPPDATA "pyhl") "snapshot"
    if (Test-Path $snapshotDir) {
        # Use fsutil file layout to get Allocated Size for each file
        $totalActual = [long]0
        $totalLogical = [long]0
        Get-ChildItem $snapshotDir -Recurse -File | ForEach-Object {
            $totalLogical += $_.Length
            # Parse 'Allocated Size' from fsutil file layout
            $layout = & fsutil file layout $_.FullName 2>$null
            $dataAlloc = $null
            $inDataStream = $false
            foreach ($line in ($layout -split "`n")) {
                $trimmed = $line.Trim()
                if ($trimmed -match '::[$]DATA$') { $inDataStream = $true }
                if ($inDataStream -and $trimmed -match '^Allocated Size\s*:\s*([\d,]+)') {
                    $dataAlloc = [long]($Matches[1] -replace ',','')
                    break
                }
            }
            if ($null -ne $dataAlloc) {
                $totalActual += $dataAlloc
            } else {
                $totalActual += $_.Length
            }
        }
        $footprint["hyperlight_snapshot"] = @{
            Path = $snapshotDir
            ActualSizeMB = [math]::Round($totalActual / 1MB, 2)
            LogicalSizeMB = [math]::Round($totalLogical / 1MB, 2)
            FileCount = (Get-ChildItem $snapshotDir -Recurse -File).Count
        }
    }

    # NanVix files (next to wxc-exec)
    $nanvixFiles = @("nanvixd.exe", "nanvix_rootfs.img", "python3.initrd")
    $nanvixTotal = 0
    $nanvixDetail = @{}
    foreach ($f in $nanvixFiles) {
        $fp = Join-Path $ExeDir $f
        if (Test-Path $fp) {
            $sz = (Get-Item $fp).Length
            $nanvixTotal += $sz
            $nanvixDetail[$f] = [math]::Round($sz / 1MB, 2)
        }
    }
    $kernelPath = Join-Path (Join-Path $ExeDir "bin") "kernel.elf"
    if (Test-Path $kernelPath) {
        $sz = (Get-Item $kernelPath).Length
        $nanvixTotal += $sz
        $nanvixDetail["bin/kernel.elf"] = [math]::Round($sz / 1MB, 2)
    }
    if ($nanvixTotal -gt 0) {
        $footprint["nanvix_files"] = @{
            TotalMB = [math]::Round($nanvixTotal / 1MB, 2)
            Files = $nanvixDetail
        }
    }

    # NanVix daemon memory (if running)
    $nanvixd = Get-Process nanvixd -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($nanvixd) {
        $footprint["nanvixd_daemon"] = @{
            PID = $nanvixd.Id
            WorkingSetMB = [math]::Round($nanvixd.WorkingSet64 / 1MB, 2)
            PeakWorkingSetMB = [math]::Round($nanvixd.PeakWorkingSet64 / 1MB, 2)
        }
    }

    # WSLc image cache size
    $wslcCache = Join-Path $env:TEMP "mxc-wslc-sessions"
    if (Test-Path $wslcCache) {
        $cacheMeasure = Get-ChildItem $wslcCache -Recurse -File | Measure-Object -Property Length -Sum
        $cacheSize = if ($cacheMeasure.Sum) { $cacheMeasure.Sum } else { 0 }
        $footprint["wslc_cache"] = @{
            Path = $wslcCache
            SizeMB = [math]::Round($cacheSize / 1MB, 2)
            FileCount = if ($cacheMeasure.Count) { $cacheMeasure.Count } else { 0 }
        }
    }

    return $footprint
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
$workloadDir = Join-Path (Join-Path (Join-Path $repoRoot "tests") "bench") "workloads"
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
    $callTimings = @()
    $restoreTimings = @()
    $memoryMBs = @()
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

        $detail = ""
        if ($result.RunnerMs -ge 0) {
            $detail += " | runner=$([math]::Round($result.RunnerMs,1))ms"
        }
        if ($result.RestoreMs -ge 0) {
            $detail += " restore=$([math]::Round($result.RestoreMs,1))ms call=$([math]::Round($result.CallMs,1))ms"
        }
        if ($result.PeakMemMB -gt 0) {
            $detail += " mem=$([math]::Round($result.PeakMemMB,1))MB"
        }
        Write-Host "$([math]::Round($result.ElapsedMs, 1)) ms$detail | $status" -ForegroundColor $color

        if (-not $isWarmup) {
            $timings += $result.ElapsedMs
            if ($result.PeakMemMB -gt 0) {
                $memoryMBs += $result.PeakMemMB
            }
            if ($result.CallMs -ge 0) {
                $callTimings += $result.CallMs
            }
            if ($result.RestoreMs -ge 0) {
                $restoreTimings += $result.RestoreMs
            }
        }
    }

    $stats = Get-Stats $timings
    $callStats = if ($callTimings.Count -gt 0) { Get-Stats $callTimings } else { $null }
    $restoreStats = if ($restoreTimings.Count -gt 0) { Get-Stats $restoreTimings } else { $null }
    $memStats = if ($memoryMBs.Count -gt 0) { Get-Stats $memoryMBs } else { $null }

    $allResults[$backend] = @{
        Backend        = $backend
        Workload       = $Workload
        Iterations     = $Iterations
        Failures       = $failures
        TimingMs       = $stats
        CallMs         = $callStats
        RestoreMs      = $restoreStats
        MemoryMB       = $memStats
        RawTimings     = $timings
    }
}

# --- Memory footprint ---

Write-Host "`n=== Measuring disk/memory footprint ===" -ForegroundColor Yellow
$footprint = Measure-BackendFootprint -ExeDir $exeDir

# --- Summary ---

Write-Host "`n" -NoNewline
Write-Host ("=" * 80) -ForegroundColor Cyan
Write-Host "  BENCHMARK RESULTS: $Workload workload ($Iterations iterations)" -ForegroundColor Cyan
Write-Host ("=" * 80) -ForegroundColor Cyan

Write-Host ""
Write-Host "  Wall-clock (total wxc-exec time):" -ForegroundColor White
$header = "{0,-15} {1,10} {2,10} {3,10} {4,10} {5,10} {6,10} {7,10}" -f `
    "Backend", "Min(ms)", "Median", "Mean", "P90", "P99", "Max(ms)", "Fails"
Write-Host $header -ForegroundColor White
Write-Host ("-" * 95)

foreach ($backend in $selectedBackends) {
    if (-not $allResults.ContainsKey($backend)) { continue }
    $r = $allResults[$backend]
    $t = $r.TimingMs

    $row = "{0,-15} {1,10} {2,10} {3,10} {4,10} {5,10} {6,10} {7,10}" -f `
        $backend, $t.Min, $t.Median, $t.Mean, $t.P90, $t.P99, $t.Max, $r.Failures
    Write-Host $row
}

if ($allResults.Values | Where-Object { $_.CallMs }) {
    Write-Host ""
    Write-Host "  Guest call time (inside the VM):" -ForegroundColor White
    $callHeader = "{0,-15} {1,10} {2,10} {3,10} {4,10} {5,10}" -f `
        "Backend", "Min(ms)", "Median", "Mean", "P90", "Max(ms)"
    Write-Host $callHeader -ForegroundColor White
    Write-Host ("-" * 70)
    foreach ($backend in $selectedBackends) {
        if (-not $allResults.ContainsKey($backend)) { continue }
        $c = $allResults[$backend].CallMs
        if ($c) {
            $row = "{0,-15} {1,10} {2,10} {3,10} {4,10} {5,10}" -f `
                $backend, $c.Min, $c.Median, $c.Mean, $c.P90, $c.Max
            Write-Host $row
        }
    }
}

if ($allResults.Values | Where-Object { $_.MemoryMB }) {
    Write-Host ""
    Write-Host "  Peak process memory (wxc-exec WorkingSet):" -ForegroundColor White
    $memHeader = "{0,-15} {1,10} {2,10} {3,10} {4,10}" -f `
        "Backend", "Min(MB)", "Median", "Mean", "Max(MB)"
    Write-Host $memHeader -ForegroundColor White
    Write-Host ("-" * 60)
    foreach ($backend in $selectedBackends) {
        if (-not $allResults.ContainsKey($backend)) { continue }
        $m = $allResults[$backend].MemoryMB
        if ($m) {
            $row = "{0,-15} {1,10} {2,10} {3,10} {4,10}" -f `
                $backend, $m.Min, $m.Median, $m.Mean, $m.Max
            Write-Host $row
        }
    }
}

Write-Host ""
Write-Host "  Disk/daemon footprint:" -ForegroundColor White
Write-Host ("-" * 60)
if ($footprint.ContainsKey("hyperlight_snapshot")) {
    $hs = $footprint["hyperlight_snapshot"]
    Write-Host "  Hyperlight snapshot:  $($hs.ActualSizeMB) MB on disk ($($hs.LogicalSizeMB) MB logical, $($hs.FileCount) files)"
}
if ($footprint.ContainsKey("nanvix_files")) {
    $nf = $footprint["nanvix_files"]
    Write-Host "  NanVix files:         $($nf.TotalMB) MB total"
    foreach ($fname in $nf.Files.Keys | Sort-Object) {
        Write-Host "    $fname = $($nf.Files[$fname]) MB"
    }
}
if ($footprint.ContainsKey("nanvixd_daemon")) {
    $nd = $footprint["nanvixd_daemon"]
    Write-Host "  nanvixd daemon (PID $($nd.PID)):  WS=$($nd.WorkingSetMB) MB  Peak=$($nd.PeakWorkingSetMB) MB"
}
if ($footprint.ContainsKey("wslc_cache")) {
    $wc = $footprint["wslc_cache"]
    Write-Host "  WSLc image cache:     $($wc.SizeMB) MB ($($wc.FileCount) files) at $($wc.Path)"
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
            callMs       = $r.CallMs
            restoreMs    = $r.RestoreMs
            memoryMB     = $r.MemoryMB
            rawTimingsMs = $r.RawTimings
        }
    }
    $jsonObj.footprint = $footprint
    $jsonObj | ConvertTo-Json -Depth 5 | Set-Content -Path $OutputJson -Encoding UTF8
    Write-Host "Results written to: $OutputJson" -ForegroundColor Green
}

# --- HTML chart output ---

if ($OutputHtml -ne "" -and $allResults.Count -gt 0) {
    $backendColors = @{
        "hyperlight" = "#3B6CE7"
        "microvm"    = "#1B9E6D"
        "wslc"       = "#C05621"
    }
    $backendLabels = @{
        "hyperlight" = "Hyperlight (Unikraft)"
        "microvm"    = "NanVix (Microvm)"
        "wslc"       = "WSLc"
    }

    # Build JS data object
    $jsData = "{"
    foreach ($backend in $selectedBackends) {
        if (-not $allResults.ContainsKey($backend)) { continue }
        $r = $allResults[$backend]
        $rawArr = ($r.RawTimings | ForEach-Object { [math]::Round($_, 2) }) -join ","
        $label = $backendLabels[$backend]
        $color = $backendColors[$backend]
        $t = $r.TimingMs
        $jsData += "`n      '$backend': {"
        $jsData += " label: '$label', color: '$color',"
        $jsData += " raw: [$rawArr],"
        $jsData += " min: $($t.Min), median: $($t.Median), mean: $($t.Mean),"
        $jsData += " p90: $($t.P90), p99: $($t.P99), max: $($t.Max), stddev: $($t.StdDev),"
        $jsData += " failures: $($r.Failures)"
        if ($r.CallMs) {
            $c = $r.CallMs
            $jsData += ", call: { min: $($c.Min), median: $($c.Median), mean: $($c.Mean), p90: $($c.P90), max: $($c.Max) }"
        }
        if ($r.RestoreMs) {
            $rs = $r.RestoreMs
            $jsData += ", restore: { min: $($rs.Min), median: $($rs.Median), mean: $($rs.Mean), p90: $($rs.P90), max: $($rs.Max) }"
        }
        if ($r.MemoryMB) {
            $m = $r.MemoryMB
            $jsData += ", memory: { min: $($m.Min), median: $($m.Median), mean: $($m.Mean), max: $($m.Max) }"
        }
        $jsData += " },"
    }
    $jsData += "`n    }"

    # Build footprint JS data
    $jsFootprint = "{"
    if ($footprint.ContainsKey("hyperlight_snapshot")) {
        $hs = $footprint["hyperlight_snapshot"]
        $jsFootprint += " hyperlight: { actualMB: $($hs.ActualSizeMB), logicalMB: $($hs.LogicalSizeMB), files: $($hs.FileCount) },"
    }
    if ($footprint.ContainsKey("nanvix_files")) {
        $nf = $footprint["nanvix_files"]
        $jsFootprint += " nanvix: { sizeMB: $($nf.TotalMB) },"
    }
    if ($footprint.ContainsKey("nanvixd_daemon")) {
        $nd = $footprint["nanvixd_daemon"]
        $jsFootprint += " nanvixd: { wsMB: $($nd.WorkingSetMB), peakMB: $($nd.PeakWorkingSetMB) },"
    }
    if ($footprint.ContainsKey("wslc_cache")) {
        $wc = $footprint["wslc_cache"]
        $jsFootprint += " wslc: { sizeMB: $($wc.SizeMB), files: $($wc.FileCount) },"
    }
    $jsFootprint += " }"

    $timestamp = Get-Date -Format "yyyy-MM-dd HH:mm"
    $hostname = $env:COMPUTERNAME

    $html = @"
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>MXC Benchmark: $Workload</title>
<style>
  :root {
    --bg: #F6F7F9; --surface: #FFF; --text: #1A1D23; --text2: #5A6172;
    --border: #D8DAE0; --grid: #E8EAEF;
    --mono: ui-monospace, "Cascadia Code", "SF Mono", Menlo, monospace;
    --sans: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #15161A; --surface: #1D1E24; --text: #E4E5EA; --text2: #8B90A0; --border: #2E3038; --grid: #252730; }
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { background: var(--bg); color: var(--text); font-family: var(--sans); line-height: 1.5; padding: 2rem 1rem; }
  .c { max-width: 860px; margin: 0 auto; }
  .eyebrow { font-size: .7rem; font-weight: 600; letter-spacing: .08em; text-transform: uppercase; color: var(--text2); margin-bottom: .25rem; }
  h1 { font-size: 1.4rem; font-weight: 700; margin-bottom: .4rem; }
  .sub { font-size: .85rem; color: var(--text2); margin-bottom: 1.5rem; }
  .cards { display: flex; gap: 1rem; flex-wrap: wrap; margin-bottom: 1.5rem; }
  .card { flex: 1; min-width: 140px; background: var(--surface); border: 1px solid var(--border); border-radius: 6px; padding: .85rem 1rem; }
  .card-label { font-size: .7rem; font-weight: 600; letter-spacing: .06em; text-transform: uppercase; color: var(--text2); }
  .card-val { font-family: var(--mono); font-size: 1.6rem; font-weight: 700; font-variant-numeric: tabular-nums; line-height: 1.3; }
  .card-note { font-size: .72rem; color: var(--text2); }
  .section { background: var(--surface); border: 1px solid var(--border); border-radius: 6px; padding: 1.25rem; margin-bottom: 1.5rem; }
  .stitle { font-size: .78rem; font-weight: 600; color: var(--text2); margin-bottom: .75rem; }
  canvas { display: block; width: 100%; height: auto; }
  .legend { display: flex; gap: 1.25rem; margin-top: .75rem; justify-content: center; flex-wrap: wrap; }
  .legend-item { display: flex; align-items: center; gap: .35rem; font-size: .78rem; color: var(--text2); }
  .legend-dot { width: 8px; height: 8px; border-radius: 50%; }
  .tbl-wrap { overflow-x: auto; }
  table { width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; }
  th { font-size: .68rem; font-weight: 600; letter-spacing: .04em; text-transform: uppercase; color: var(--text2); text-align: right; padding: .45rem .75rem; border-bottom: 1px solid var(--border); white-space: nowrap; }
  th:first-child { text-align: left; }
  td { font-family: var(--mono); font-size: .8rem; text-align: right; padding: .55rem .75rem; border-bottom: 1px solid var(--grid); white-space: nowrap; }
  td:first-child { font-family: var(--sans); font-weight: 600; text-align: left; }
  tr:last-child td { border-bottom: none; }
  footer { text-align: center; font-size: .7rem; color: var(--text2); padding-top: .5rem; }
</style>
</head>
<body>
<div class="c">
  <div class="eyebrow">MXC Containment Benchmark</div>
  <h1>$Workload workload &mdash; Wall-Clock Execution</h1>
  <div class="sub">$Iterations iterations &middot; $hostname &middot; $timestamp</div>
  <div class="cards" id="cards"></div>
  <div class="section">
    <div class="stitle">Distribution of wall-clock times (ms)</div>
    <canvas id="strip" width="1520" height="200"></canvas>
    <div class="legend" id="legend1"></div>
  </div>
  <div class="section">
    <div class="stitle">Per-iteration comparison</div>
    <canvas id="line" width="1520" height="360"></canvas>
    <div class="legend" id="legend2"></div>
  </div>
  <div class="section" style="padding:0; overflow:hidden;">
    <div style="padding:.85rem 1rem .5rem; font-size:.78rem; font-weight:600; color:var(--text2);">Summary (ms)</div>
    <div class="tbl-wrap"><table id="stats"></table></div>
  </div>
  <div class="section" style="padding:0; overflow:hidden;">
    <div style="padding:.85rem 1rem .5rem; font-size:.78rem; font-weight:600; color:var(--text2);">Memory &amp; Disk Footprint</div>
    <div class="tbl-wrap"><table id="footprintTbl"></table></div>
  </div>
  <footer>wxc-exec release build &middot; WHP snapshots</footer>
</div>
<script>
const D = $jsData;
const FP = $jsFootprint;
const BACKENDS = Object.keys(D);
function css(p) { return getComputedStyle(document.documentElement).getPropertyValue(p).trim(); }

// Cards
(function() {
  const el = document.getElementById('cards');
  BACKENDS.forEach(k => {
    const d = D[k];
    const card = document.createElement('div');
    card.className = 'card';
    card.innerHTML = '<div class="card-label">' + d.label + '</div>'
      + '<div class="card-val" style="color:' + d.color + '">' + d.median + '<span style="font-size:.7rem;opacity:.7"> ms</span></div>'
      + '<div class="card-note">median wall-clock' + (d.failures > 0 ? ' &middot; ' + d.failures + ' failures' : '') + '</div>';
    el.appendChild(card);
  });
})();

function setupCanvas(id) {
  const c = document.getElementById(id);
  const dpr = window.devicePixelRatio || 1;
  const w = c.width, h = c.height;
  c.style.width = (w/2) + 'px'; c.style.height = (h/2) + 'px';
  c.width = w * dpr; c.height = h * dpr;
  const ctx = c.getContext('2d'); ctx.scale(dpr, dpr);
  return { ctx, W: w, H: h };
}

function legend(id) {
  const el = document.getElementById(id);
  BACKENDS.forEach(k => {
    const d = D[k];
    const item = document.createElement('div'); item.className = 'legend-item';
    item.innerHTML = '<div class="legend-dot" style="background:' + d.color + '"></div>' + d.label;
    el.appendChild(item);
  });
}

function drawStrip() {
  const { ctx, W, H } = setupCanvas('strip');
  const pad = { top: 20, right: 40, bottom: 40, left: 110 };
  const allVals = BACKENDS.flatMap(k => D[k].raw);
  const xMin = Math.floor(Math.min(...allVals) / 50) * 50 - 20;
  const xMax = Math.ceil(Math.max(...allVals) / 50) * 50 + 20;
  const plotW = W - pad.left - pad.right;
  const rowH = (H - pad.top - pad.bottom) / BACKENDS.length;
  const xScale = v => pad.left + ((v - xMin) / (xMax - xMin)) * plotW;

  // grid
  const gridColor = css('--grid'), text2 = css('--text2');
  ctx.strokeStyle = gridColor; ctx.lineWidth = 1;
  const step = xMax - xMin > 500 ? 500 : xMax - xMin > 200 ? 100 : 20;
  for (let v = Math.ceil(xMin/step)*step; v <= xMax; v += step) {
    const x = Math.round(xScale(v)) + 0.5;
    ctx.beginPath(); ctx.moveTo(x, pad.top); ctx.lineTo(x, H - pad.bottom); ctx.stroke();
  }
  ctx.font = '11px ' + css('--mono'); ctx.fillStyle = text2; ctx.textAlign = 'center'; ctx.textBaseline = 'top';
  for (let v = Math.ceil(xMin/step)*step; v <= xMax; v += step) ctx.fillText(v+'', xScale(v), H - pad.bottom + 6);
  ctx.font = '11px ' + css('--sans'); ctx.fillText('Wall-clock (ms)', pad.left + plotW/2, H - pad.bottom + 22);

  BACKENDS.forEach((k, i) => {
    const d = D[k], cy = pad.top + rowH * (i + 0.5), bandH = Math.min(28, rowH * 0.6);
    // label
    ctx.font = '600 12px ' + css('--sans'); ctx.fillStyle = d.color;
    ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
    ctx.fillText(d.label.split('(')[0].trim(), pad.left - 10, cy);
    // range band
    const rMin = Math.min(...d.raw), rMax = Math.max(...d.raw);
    ctx.fillStyle = d.color + '20';
    ctx.beginPath();
    const bx = xScale(rMin), bw = xScale(rMax) - bx;
    ctx.roundRect(bx, cy - bandH/2, bw, bandH, 3); ctx.fill();
    // median line
    ctx.strokeStyle = d.color; ctx.globalAlpha = .5; ctx.lineWidth = 2; ctx.setLineDash([4,3]);
    ctx.beginPath(); ctx.moveTo(xScale(d.median), cy - bandH/2 - 4); ctx.lineTo(xScale(d.median), cy + bandH/2 + 4); ctx.stroke();
    ctx.globalAlpha = 1; ctx.setLineDash([]);
    // dots
    d.raw.forEach((v, j) => {
      const jitter = (j % 3 - 1) * 4;
      ctx.beginPath(); ctx.arc(xScale(v), cy + jitter, 4, 0, Math.PI*2);
      ctx.fillStyle = d.color; ctx.fill();
    });
    // median label
    ctx.font = '600 10px ' + css('--mono'); ctx.fillStyle = d.color;
    ctx.textAlign = 'center'; ctx.textBaseline = 'bottom';
    ctx.fillText(d.median + ' ms', xScale(d.median), cy - bandH/2 - 7);
  });
  legend('legend1');
}

function drawLine() {
  const { ctx, W, H } = setupCanvas('line');
  const pad = { top: 20, right: 30, bottom: 48, left: 55 };
  const plotW = W - pad.left - pad.right, plotH = H - pad.top - pad.bottom;
  const allVals = BACKENDS.flatMap(k => D[k].raw);
  const yMin = Math.floor(Math.min(...allVals) / 50) * 50 - 20;
  const yMax = Math.ceil(Math.max(...allVals) / 50) * 50 + 20;
  const n = Math.max(...BACKENDS.map(k => D[k].raw.length));
  const xPos = i => pad.left + (i / (n - 1)) * plotW;
  const yPos = v => pad.top + plotH - ((v - yMin) / (yMax - yMin)) * plotH;

  const gridColor = css('--grid'), text2 = css('--text2');
  // y grid
  ctx.strokeStyle = gridColor; ctx.lineWidth = 1;
  const yStep = yMax - yMin > 1000 ? 500 : yMax - yMin > 200 ? 100 : 20;
  ctx.font = '11px ' + css('--mono'); ctx.fillStyle = text2;
  ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
  for (let v = Math.ceil(yMin/yStep)*yStep; v <= yMax; v += yStep) {
    const y = Math.round(yPos(v)) + 0.5;
    ctx.beginPath(); ctx.moveTo(pad.left, y); ctx.lineTo(W - pad.right, y); ctx.stroke();
    ctx.fillText(v+'', pad.left - 7, y);
  }
  // x labels
  ctx.textAlign = 'center'; ctx.textBaseline = 'top';
  for (let i = 0; i < n; i++) ctx.fillText('#'+(i+1), xPos(i), H - pad.bottom + 7);
  ctx.font = '11px ' + css('--sans'); ctx.fillText('Iteration', pad.left + plotW/2, H - pad.bottom + 24);

  BACKENDS.forEach(k => {
    const d = D[k];
    // area
    ctx.beginPath(); ctx.moveTo(xPos(0), yPos(yMin));
    d.raw.forEach((v, i) => ctx.lineTo(xPos(i), yPos(v)));
    ctx.lineTo(xPos(d.raw.length - 1), yPos(yMin)); ctx.closePath();
    ctx.fillStyle = d.color + '18'; ctx.fill();
    // line
    ctx.beginPath(); d.raw.forEach((v, i) => { if (i === 0) ctx.moveTo(xPos(0), yPos(v)); else ctx.lineTo(xPos(i), yPos(v)); });
    ctx.strokeStyle = d.color; ctx.lineWidth = 2.5; ctx.lineJoin = 'round'; ctx.stroke();
    // dots
    d.raw.forEach((v, i) => {
      ctx.beginPath(); ctx.arc(xPos(i), yPos(v), 3.5, 0, Math.PI*2);
      ctx.fillStyle = css('--surface'); ctx.fill();
      ctx.strokeStyle = d.color; ctx.lineWidth = 2; ctx.stroke();
    });
  });
  legend('legend2');
}

// Table
(function() {
  const tbl = document.getElementById('stats');
  let h = '<thead><tr><th>Backend</th><th>Min</th><th>Median</th><th>Mean</th><th>P90</th><th>P99</th><th>Max</th><th>StdDev</th><th>Fails</th></tr></thead><tbody>';
  BACKENDS.forEach(k => {
    const d = D[k];
    h += '<tr><td style="color:' + d.color + '">' + d.label + '</td>';
    h += '<td>' + d.min + '</td><td>' + d.median + '</td><td>' + d.mean + '</td>';
    h += '<td>' + d.p90 + '</td><td>' + d.p99 + '</td><td>' + d.max + '</td>';
    h += '<td>' + d.stddev + '</td><td>' + d.failures + '</td></tr>';
  });
  // Add memory column if any backend has it
  const hasMemory = BACKENDS.some(k => D[k].memory);
  if (hasMemory) {
    h += '<tr><td colspan="9" style="font-weight:600;font-family:var(--sans);padding-top:1rem;">Peak Process Memory (MB)</td></tr>';
    BACKENDS.forEach(k => {
      const d = D[k];
      if (d.memory) {
        h += '<tr><td style="color:' + d.color + '">' + d.label + '</td>';
        h += '<td>' + d.memory.min + '</td><td>' + d.memory.median + '</td><td>' + d.memory.mean + '</td>';
        h += '<td></td><td></td><td>' + d.memory.max + '</td><td></td><td></td></tr>';
      }
    });
  }
  h += '</tbody>';
  tbl.innerHTML = h;
})();

// Footprint table
(function() {
  const tbl = document.getElementById('footprintTbl');
  let h = '<thead><tr><th>Component</th><th>Size (MB)</th><th>Detail</th></tr></thead><tbody>';
  if (FP.hyperlight) h += '<tr><td>Hyperlight snapshot</td><td>' + FP.hyperlight.actualMB + '</td><td>on disk (' + FP.hyperlight.logicalMB + ' MB logical, ' + FP.hyperlight.files + ' files, sparse)</td></tr>';
  if (FP.nanvix) h += '<tr><td>NanVix files</td><td>' + FP.nanvix.sizeMB + '</td><td>rootfs + initrd + kernel + daemon</td></tr>';
  if (FP.nanvixd) h += '<tr><td>nanvixd daemon (live)</td><td>' + FP.nanvixd.wsMB + '</td><td>working set (peak ' + FP.nanvixd.peakMB + ' MB)</td></tr>';
  if (FP.wslc) h += '<tr><td>WSLc image cache</td><td>' + FP.wslc.sizeMB + '</td><td>' + FP.wslc.files + ' files</td></tr>';
  h += '</tbody>';
  tbl.innerHTML = h;
})();

drawStrip();
drawLine();
</script>
</body>
</html>
"@

    Set-Content -Path $OutputHtml -Value $html -Encoding UTF8
    Write-Host "HTML report written to: $OutputHtml" -ForegroundColor Green
}
