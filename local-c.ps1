<#
.SYNOPSIS
    Local build and quality gate for alexa-mcp.

.DESCRIPTION
    Runs the same checks as CI (format, type-check, clippy, tests) and can
    optionally build the release binary or the Docker image, so a failure is
    caught before pushing.

.EXAMPLE
    ./local-c.ps1
    ./local-c.ps1 -Release
    ./local-c.ps1 -Docker -Tag alexa-mcp:dev
#>
[CmdletBinding()]
param(
    # Build the optimized release binary after the checks.
    [switch]$Release,
    # Build the Docker image after the checks.
    [switch]$Docker,
    # Skip cargo test.
    [switch]$SkipTests,
    # Docker image tag used with -Docker.
    [string]$Tag = "alexa-mcp:local"
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $root
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo not found. Install Rust from https://rustup.rs first."
    }

    # On Windows the MSVC toolchain sometimes fails to compile C dependencies
    # (ring) with debug info (cl.exe D8050). Disabling debug info makes local
    # builds reliable and faster; it does not change the released binary.
    if ($env:OS -eq 'Windows_NT') {
        $env:CARGO_PROFILE_DEV_DEBUG = '0'
    }

    function Invoke-Step {
        param([string]$Name, [scriptblock]$Action)
        Write-Host "==> $Name" -ForegroundColor Cyan
        & $Action
        if ($LASTEXITCODE -ne 0) {
            throw "$Name failed with exit code $LASTEXITCODE"
        }
    }

    Invoke-Step 'cargo fmt --all -- --check' { cargo fmt --all -- --check }
    Invoke-Step 'cargo check --all-targets' { cargo check --all-targets }
    Invoke-Step 'cargo clippy --all-targets' { cargo clippy --all-targets -- -D warnings }

    if (-not $SkipTests) {
        Invoke-Step 'cargo test --all-targets' { cargo test --all-targets }
    }

    if ($Release) {
        Invoke-Step 'cargo build --release' { cargo build --release }
    }

    if ($Docker) {
        if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
            throw "docker not found but -Docker was requested."
        }
        Invoke-Step "docker build -t $Tag ." { docker build -t $Tag . }
    }

    Write-Host "All checks passed." -ForegroundColor Green
}
finally {
    Pop-Location
}
