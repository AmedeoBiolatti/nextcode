# Builds the Linux agent (next_code_agent) inside WSL and copies it next to
# next_code.exe, where the editor looks for it when opening a WSL project.
#
#   scripts\build-wsl-agent.ps1                  # release, default distro
#   scripts\build-wsl-agent.ps1 -Profile debug -Distro Ubuntu
#
# Needs Rust (rustup) inside the distro. The agent is linked statically
# (musl), so one build runs in every x86_64 distro.

param(
    [string]$Distro = "",
    [ValidateSet("release", "debug")]
    [string]$Profile = "release"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$target = "x86_64-unknown-linux-musl"
$distroArgs = if ($Distro) { @("-d", $Distro) } else { @() }

$linuxRepo = (& wsl.exe @distroArgs --exec wslpath -a ($repo -replace '\\', '/')).Trim()
if ($LASTEXITCODE -ne 0) { throw "Cannot map $repo into WSL" }

$cargoProfile = if ($Profile -eq "release") { "--release" } else { "" }
# A separate target dir keeps Linux build artifacts away from the Windows ones.
$build = "rustup target add $target >/dev/null && CARGO_TARGET_DIR=target/wsl cargo build $cargoProfile -p next_code_agent --target $target"
& wsl.exe @distroArgs --cd $linuxRepo --exec bash -lc $build
if ($LASTEXITCODE -ne 0) { throw "Building the agent in WSL failed" }

$built = Join-Path $repo "target\wsl\$target\$Profile\next_code_agent"
$destination = Join-Path $repo "target\$Profile"
New-Item -ItemType Directory -Force -Path $destination | Out-Null
Copy-Item -Force $built (Join-Path $destination "next_code_agent")
Write-Host "Agent copied to $destination\next_code_agent"
