<#
.SYNOPSIS
    Capture screenshots for MRP packages.

.DESCRIPTION
    Runs the standalone emulator headlessly for every .mrp package under
    the selected game directory. PNG screenshots are written to
    docs/images. When no binary is supplied, the latest release binary is
    built before capture.

.PARAMETER Frames
    Number of frames to run before capturing. Default: 200. Selected
    applications use capture-specific frame counts.

.PARAMETER InstructionLimit
    Total guest instruction budget for boot and each capture. Default:
    512000000 (0 disables the cap).

.PARAMETER Binary
    Path to the mrpemu executable. Default: the release build output.

.PARAMETER GameDirectory
    Directory searched recursively for .mrp packages.

.PARAMETER OutputDirectory
    Directory where PNG screenshots are written.

.PARAMETER SystemDirectory
    Optional directory with Mythroad device files, passed as --system-dir.
    The emulator defaults to each package's parent directory when omitted.

.PARAMETER Press
    Repeatable frame:key schedule applied to every package without its own
    recipe, for example 30:lsk 60:lsk. Use an empty per-title recipe Press
    array to capture that title without presses.
#>

param(
    [ValidateRange(0, [int]::MaxValue)]
    [int]$Frames = 200,
    [ValidateRange(0, [uint64]::MaxValue)]
    [uint64]$InstructionLimit = 512000000,
    [string]$Binary = "",
    [Parameter(Mandatory)]
    [string]$GameDirectory,
    [string]$OutputDirectory = "",
    [string]$SystemDirectory = "",
    [string[]]$Press = @()
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repoRoot "docs\images"
}
if (-not (Test-Path -LiteralPath $GameDirectory -PathType Container)) {
    Write-Error "Game directory not found: $GameDirectory"
    exit 1
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

if (-not $Binary) {
    $Binary = Join-Path $repoRoot "target\release\mrpemu.exe"
    if ($env:OS -ne "Windows_NT") {
        $Binary = Join-Path $repoRoot "target/release/mrpemu"
    }

    Write-Host "Building the latest release binary..." -ForegroundColor Yellow
    try {
        Push-Location $repoRoot
        cargo build --release -p mrpemu --bin mrpemu
        if ($LASTEXITCODE -ne 0) {
            throw "Release build failed with exit code $LASTEXITCODE."
        }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    Write-Error "mrpemu binary not found: $Binary"
    exit 1
}

# Per-title bring-up recipes: the scripted key presses (frame:key) needed to
# reach a representative screen. Keys: up, down, left, right, ok, lsk, rsk,
# 0-9, star, pound. Titles not listed use the default frame count and the
# global -Press schedule. Add an override here after verifying a different
# sequence in docs/Game-Compatibility.md.
$recipes = @{
    # "银河争霸" = @{ Frames = 200; Press = @("30:ok", "90:ok") }

    # The six-press bring-up stops before these titles reach a better screen,
    # or faults inside the guest. Keep their idle 200-frame capture instead.
    "Super赛亚人传说（已破解）(1)" = @{ Frames = 200; Press = @() }
    "Super赛亚人传说（已破解）" = @{ Frames = 200; Press = @() }
    "大鱼吃小鱼-破解版无rc" = @{ Frames = 200; Press = @() }
    "山脊赛车-无rc破解版" = @{ Frames = 200; Press = @() }
    "战国英雄传" = @{ Frames = 200; Press = @() }
    "指环王之霍比特人正版（240x400）" = @{ Frames = 200; Press = @() }
}

$games = @(Get-ChildItem -LiteralPath $GameDirectory -Recurse -File |
    Where-Object { $_.Extension -ieq ".mrp" } |
    Sort-Object FullName)

if ($games.Count -eq 0) {
    Write-Warning "No .mrp packages found under $GameDirectory"
    exit 0
}

function Get-SafeBaseName {
    param(
        [Parameter(Mandatory)]
        [string]$BaseName
    )

    $safeName = $BaseName -replace '[<>:"/\\|?*\x00-\x1F]', '_'
    $safeName = $safeName.Trim().TrimEnd('.')
    if (-not $safeName) {
        return "unnamed"
    }
    return $safeName
}

Write-Host "Using binary: $Binary"
Write-Host "Game dir:     $GameDirectory"
Write-Host "Output dir:   $OutputDirectory"
Write-Host "Frames:       $Frames"
Write-Host "Games:        $($games.Count)"
Write-Host ""

$success = 0
$warned = 0
$failed = 0
$usedNames = [System.Collections.Generic.Dictionary[string, int]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
$corpusRoot = (Resolve-Path -LiteralPath $GameDirectory).Path.TrimEnd([char[]]"\/")

foreach ($game in $games) {
    $baseName = [System.IO.Path]::GetFileNameWithoutExtension($game.Name)
    $recipe = if ($recipes.ContainsKey($baseName)) { $recipes[$baseName] } else { @{} }
    $captureFrames = if ($recipe.ContainsKey("Frames")) { $recipe["Frames"] } else { $Frames }
    $capturePress = if ($recipe.ContainsKey("Press")) { $recipe["Press"] } else { $Press }
    $frameLabel = if ($captureFrames -eq 1) { "1 frame" } else { "$captureFrames frames" }

    $safeName = Get-SafeBaseName $baseName
    $imageDirectory = $OutputDirectory
    $group = ""
    if ($game.FullName.StartsWith($corpusRoot + "\", [StringComparison]::OrdinalIgnoreCase)) {
        $relativeGamePath = $game.FullName.Substring($corpusRoot.Length + 1)
        $group = ($relativeGamePath -split '[/\\]')[0]
    }
    if ($group -in @("A", "B", "C")) {
        $imageDirectory = Join-Path $OutputDirectory $group
        New-Item -ItemType Directory -Force -Path $imageDirectory | Out-Null
    }
    $imageKey = Join-Path $imageDirectory $safeName
    if ($usedNames.ContainsKey($imageKey)) {
        $usedNames[$imageKey]++
        $safeName = "$safeName-$($usedNames[$imageKey])"
    } else {
        $usedNames[$imageKey] = 1
    }

    $imageName = "$safeName.png"
    $outPath = Join-Path $imageDirectory $imageName
    Write-Host -NoNewline "  $baseName ($frameLabel) ... "

    # The frontend overwrites the PNG in place. Record the start time so a
    # stale file cannot be reported as the current result.
    $captureStarted = Get-Date

    $status = "Fail"
    $detail = "Execution failed"
    $nonzero = $null
    $colors = $null
    $exitCode = -1
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $captureArgs = @($game.FullName,
            "--screenshot", $outPath,
            "--screenshot-frames", $captureFrames,
            "--instruction-limit", $InstructionLimit)
        if ($SystemDirectory) {
            $captureArgs += @("--system-dir", $SystemDirectory)
        }
        # PowerShell variables are case-insensitive: never bind $press here,
        # or the global -Press parameter is overwritten after the first game.
        foreach ($pressSpec in $capturePress) {
            $captureArgs += @("--press", $pressSpec)
        }
        $output = & $Binary @captureArgs 2>&1
        $exitCode = $LASTEXITCODE
        $diagnostics = $output | Out-String
        $diagnostics = [regex]::Replace(
            $diagnostics,
            '\x1B\[[0-?]*[ -/]*[@-~]',
            ''
        )
        if ($diagnostics -match 'frame nonzero=(\d+) colors=(\d+)') {
            $nonzero = [int]$Matches[1]
            $colors = [int]$Matches[2]
        }

        if ($exitCode -ne 0) {
            $diagnosticLines = @($diagnostics -split '\r?\n' |
                ForEach-Object { $_.Trim() } |
                Where-Object { $_ })
            if ($diagnosticLines.Count -gt 0) {
                $detail = $diagnosticLines[$diagnosticLines.Count - 1]
                $detail = $detail -replace '^\d+:\s*', ''
                $detail = $detail -replace '^Error:\s*', ''
            } else {
                $detail = "Exited with code $exitCode"
            }
            Write-Host $detail -ForegroundColor Red
        } elseif (-not (Test-Path -LiteralPath $outPath -PathType Leaf)) {
            $detail = "No screenshot produced"
            Write-Host $detail -ForegroundColor Red
        } elseif ((Get-Item -LiteralPath $outPath).LastWriteTime -lt $captureStarted.AddSeconds(-2)) {
            $status = "Warn"
            $detail = "Stale screenshot file"
            Write-Host $detail -ForegroundColor Yellow
        } elseif ($null -eq $nonzero -or $null -eq $colors) {
            $status = "Warn"
            $detail = "Frame statistics unavailable"
            Write-Host $detail -ForegroundColor Yellow
        } elseif ($nonzero -eq 0 -or $colors -le 1) {
            $status = "Warn"
            $detail = "Blank or single-color frame"
            Write-Host "$detail ($colors colors)" -ForegroundColor Yellow
        } else {
            $status = "Pass"
            $detail = "Rendered $colors colors"
            $size = (Get-Item -LiteralPath $outPath).Length
            Write-Host "OK ($colors colors, $([math]::Round($size / 1KB)) KB)" -ForegroundColor Green
        }
    } catch {
        $detail = $_.Exception.Message
        Write-Host "FAILED ($detail)" -ForegroundColor Red
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    switch ($status) {
        "Pass" { $success++ }
        "Warn" { $warned++ }
        default { $failed++ }
    }
}

Write-Host ""
Write-Host "Done: $success succeeded, $warned warned, $failed failed out of $($games.Count) total."
