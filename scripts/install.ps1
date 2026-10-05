# Checks what agent-pr-runner needs, then builds it and installs it outside any repository.
#
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1              # check, build, install
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -CheckOnly   # only check
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -InstallDir D:\tools\apr
#
# Creates:  <InstallDir>\agent-pr-runner.exe
#           <InstallDir>\disabled-hooks\   (must stay empty)
#           <InstallDir>\queues\           (one folder per repository)
#           <InstallDir>\configs\          (one config per repository)
# Re-running it updates the binary and leaves configs and queues alone.
#
# Exit codes: 0 done, 2 something is missing (the report says what to install and how).

param(
    [string]$InstallDir = (Join-Path $HOME "agent-pr-runner"),
    [switch]$CheckOnly
)

$ErrorActionPreference = "Stop"
$source = Split-Path -Parent $PSScriptRoot

# Pick up tools installed since this terminal opened (winget and rustup change the saved PATH,
# not the PATH of terminals that are already running).
$saved = @(
    [Environment]::GetEnvironmentVariable("Path", "Machine"),
    [Environment]::GetEnvironmentVariable("Path", "User"),
    (Join-Path $HOME ".cargo\bin")
) | Where-Object { $_ }
$env:Path = (@($env:Path) + $saved) -join ";"

function Find-Tool([string]$name) {
    $cmd = Get-Command $name -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return $cmd.Source }
    return $null
}

$missing = @()
Write-Host "Checking what agent-pr-runner needs:"

$tools = @(
    @{ Name = "cargo"; What = "Rust"; Install = "winget install --id Rustlang.Rustup -e   (then accept the Visual Studio C++ Build Tools if rustup offers them; or see https://rustup.rs)" },
    @{ Name = "git"; What = "Git"; Install = "winget install --id Git.Git -e   (or https://git-scm.com)" },
    @{ Name = "gh"; What = "GitHub CLI"; Install = "winget install --id GitHub.cli -e   (or https://cli.github.com)" }
)
foreach ($tool in $tools) {
    $path = Find-Tool $tool.Name
    if ($path) {
        Write-Host ("  ok       {0,-11} {1}" -f $tool.What, $path)
    } else {
        Write-Host ("  MISSING  {0,-11} install: {1}" -f $tool.What, $tool.Install)
        $missing += $tool.What
    }
}

if (Find-Tool "gh") {
    # gh reports on stderr; keep that from stopping the script.
    $ErrorActionPreference = "Continue"
    & (Find-Tool "gh") auth status 2>&1 | Out-Null
    $loggedIn = $LASTEXITCODE -eq 0
    $ErrorActionPreference = "Stop"
    if ($loggedIn) {
        Write-Host "  ok       gh login    logged in"
    } else {
        Write-Host "  MISSING  gh login    run: gh auth login   (the user does this; it opens a browser)"
        $missing += "gh login"
    }
}

if ($missing.Count -gt 0) {
    Write-Host ""
    Write-Host "Missing: $($missing -join ', ')."
    Write-Host "Install them, open a new terminal so PATH updates, and run this script again."
    exit 2
}
if ($CheckOnly) {
    Write-Host ""
    Write-Host "Everything needed is installed."
    exit 0
}

Push-Location $source
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

foreach ($dir in @($InstallDir, (Join-Path $InstallDir "disabled-hooks"), (Join-Path $InstallDir "queues"), (Join-Path $InstallDir "configs"))) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
}

$exe = Join-Path $InstallDir "agent-pr-runner.exe"
if (Test-Path $exe) {
    Copy-Item $exe "$exe.previous" -Force
}
Copy-Item (Join-Path $source "target\release\agent-pr-runner.exe") $exe -Force

Write-Host ""
Write-Host "Installed: $exe"
Write-Host "Next, write the config for a repository:"
Write-Host "  & `"$exe`" init --repo <path to the repository>"
