# cloudcode installer for Windows (via WSL2).
#
# Native Windows builds don't exist yet — the agent depends on tmux
# for session persistence and Seatbelt/Linux namespaces for the
# sandbox. This script gets you up and running by:
#
#   1. Ensuring WSL2 is installed with a Linux distro
#   2. Running the Linux install.sh inside WSL2
#
# Usage (from PowerShell):
#   iwr -useb https://raw.githubusercontent.com/initialz/cloudcode/main/install.ps1 | iex
#
# Or with explicit component (default: agent):
#   $env:CC_COMPONENT="agent"; iwr -useb https://raw.githubusercontent.com/initialz/cloudcode/main/install.ps1 | iex

#Requires -Version 5.1

$ErrorActionPreference = 'Stop'

# ── Component selection ──────────────────────────────────────────────────────

$Component = if ($env:CC_COMPONENT) { $env:CC_COMPONENT } else { 'agent' }
if ($Component -notin @('hub','agent','client')) {
    Write-Host "Invalid CC_COMPONENT '$Component'. Must be one of: hub, agent, client." -ForegroundColor Red
    exit 1
}

$Repo = 'initialz/cloudcode'
$InstallScriptUrl = "https://raw.githubusercontent.com/$Repo/main/install.sh"

Write-Host ""
Write-Host "═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host " CloudCode Windows Installer (via WSL2)" -ForegroundColor Cyan
Write-Host " Component: $Component" -ForegroundColor Cyan
Write-Host "═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host ""

# ── Step 1: Check WSL is available ───────────────────────────────────────────

function Test-CommandExists {
    param([string]$Name)
    return [bool] (Get-Command $Name -ErrorAction SilentlyContinue)
}

if (-not (Test-CommandExists 'wsl')) {
    Write-Host "WSL is not installed on this machine." -ForegroundColor Yellow
    Write-Host ""
    Write-Host "Run this in an elevated PowerShell (Administrator), then reboot:" -ForegroundColor White
    Write-Host "  wsl --install" -ForegroundColor Green
    Write-Host ""
    Write-Host "After reboot, set up your Linux username/password when prompted,"
    Write-Host "then re-run this installer."
    exit 1
}

# ── Step 2: Check WSL2 + a default distro ────────────────────────────────────

Write-Host "[1/3] Checking WSL state..." -ForegroundColor White

# `wsl -l -v` output uses UTF-16LE — coerce to UTF-8 for matching.
$prevEnc = [Console]::OutputEncoding
[Console]::OutputEncoding = [System.Text.Encoding]::Unicode
$wslList = & wsl.exe -l -v 2>&1
[Console]::OutputEncoding = $prevEnc

if ($LASTEXITCODE -ne 0 -or -not $wslList) {
    Write-Host "  No Linux distribution is installed." -ForegroundColor Yellow
    Write-Host ""
    Write-Host "Install Ubuntu (in an elevated PowerShell):" -ForegroundColor White
    Write-Host "  wsl --install -d Ubuntu" -ForegroundColor Green
    Write-Host ""
    Write-Host "Follow the prompts to create your Linux username and password,"
    Write-Host "then re-run this installer."
    exit 1
}

# Pick the default distro: the one marked with '*'. Lines look like:
#   * Ubuntu    Running    2
$defaultLine = $wslList | Where-Object { $_ -match '^\*' } | Select-Object -First 1
if (-not $defaultLine) {
    Write-Host "  Could not determine your default WSL distro." -ForegroundColor Red
    Write-Host "  Set one with: wsl --set-default <Name>"
    exit 1
}

# Parse "* Name State Version"
$parts = ($defaultLine -replace '^\*\s+', '') -split '\s+' | Where-Object { $_ -ne '' }
$DistroName = $parts[0]
$WslVersion = $parts[-1]

if ($WslVersion -ne '2') {
    Write-Host "  Default distro '$DistroName' is on WSL$WslVersion." -ForegroundColor Yellow
    Write-Host "  CloudCode requires WSL2. Upgrade with:" -ForegroundColor White
    Write-Host "    wsl --set-version $DistroName 2" -ForegroundColor Green
    exit 1
}

Write-Host "  Using WSL2 distro: $DistroName" -ForegroundColor Green

# ── Step 3: Run install.sh inside WSL ────────────────────────────────────────

Write-Host ""
Write-Host "[2/3] Running Linux installer inside WSL..." -ForegroundColor White
Write-Host "  → curl -fsSL $InstallScriptUrl | sh -s -- $Component"
Write-Host ""

# The Linux install.sh installs to /usr/local/bin by default, which needs
# sudo inside WSL. We let install.sh handle the sudo prompt itself by
# invoking it through bash -lc so the user sees the password prompt.
$cmd = "curl -fsSL $InstallScriptUrl | sudo sh -s -- $Component"

& wsl.exe -d $DistroName -- bash -lc $cmd
$wslExit = $LASTEXITCODE
if ($wslExit -ne 0) {
    Write-Host ""
    Write-Host "WSL install command exited with code $wslExit." -ForegroundColor Red
    exit $wslExit
}

# ── Step 4: Final instructions ───────────────────────────────────────────────

Write-Host ""
Write-Host "[3/3] Done!" -ForegroundColor Green
Write-Host ""

if ($Component -eq 'agent') {
    Write-Host "Next steps:" -ForegroundColor Cyan
    Write-Host "  1. Open WSL:           wsl -d $DistroName"
    Write-Host "  2. Make sure claude is installed in WSL (claude --version)."
    Write-Host "     If not: https://docs.claude.com/en/docs/claude-code/setup"
    Write-Host "  3. Initialize agent:   cloudcode-agent --init"
    Write-Host "  4. Edit agent.toml to set [auth].registration_token from your hub"
    Write-Host "  5. Start the agent:    cloudcode-agent daemon start --config ./agent.toml"
} elseif ($Component -eq 'hub') {
    Write-Host "Next steps:" -ForegroundColor Cyan
    Write-Host "  1. Open WSL:           wsl -d $DistroName"
    Write-Host "  2. Initialize hub:     cloudcode-hub --init"
    Write-Host "  3. Save the printed tokens (agent + admin)"
    Write-Host "  4. Start the hub:      cloudcode-hub daemon start --config ./hub.toml"
} else {
    Write-Host "Next steps:" -ForegroundColor Cyan
    Write-Host "  1. Open WSL:           wsl -d $DistroName"
    Write-Host "  2. Initialize client:  cloudcode --init"
    Write-Host "  3. Edit ~/.config/cloudcode/config.toml"
    Write-Host "  4. Run:                cloudcode"
}

Write-Host ""
