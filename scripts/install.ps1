# Copies the release build (launcher, editor, grammar libraries, Linux
# agent) to a folder outside the cargo target directory, so the editor can
# stay running between launches (`keep_running`) without holding the files
# cargo rewrites. Run after `cargo build --release`.
#
#   .\scripts\install.ps1 [-Destination <folder>] [-Profile release|dist]
#   (default: %LOCALAPPDATA%\next_code\bin, from target\release; `-Profile dist`
#   installs `cargo build --profile dist`, the smaller fat-LTO build)
param(
    [string]$Destination = (Join-Path $env:LOCALAPPDATA "next_code\bin"),
    [string]$Profile = "release"
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$release = Join-Path $root "target\$Profile"
$files = @("next_code.exe", "next_code_editor.exe")
foreach ($file in $files) {
    if (-not (Test-Path (Join-Path $release $file))) {
        throw "$file is missing in $release; run cargo build --profile $Profile first"
    }
}
# The installed editor may be running (resident): ask it to exit and wait.
$launcher = Join-Path $Destination "next_code.exe"
if (Test-Path $launcher) {
    & $launcher --quit 2>$null
    for ($i = 0; $i -lt 50; $i++) {
        if (-not (Get-Process next_code_editor -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 100
    }
}
New-Item -ItemType Directory -Force -Path $Destination | Out-Null
foreach ($file in $files + @("next_code_agent")) {
    $source = Join-Path $release $file
    if (Test-Path $source) {
        Copy-Item $source (Join-Path $Destination $file) -Force
        Write-Host "installed $file"
    }
}
# The grammar libraries, loaded on first use: cargo leaves those built as
# dependencies in `deps` (`cargo build --release --workspace` also puts
# them next to the executable).
$grammars = @(Get-ChildItem (Join-Path $release "deps") -Filter "nc_grammar_*.dll") +
    @(Get-ChildItem $release -Filter "nc_grammar_*.dll")
$seen = @{}
foreach ($grammar in $grammars) {
    if ($seen.ContainsKey($grammar.Name)) { continue }
    $seen[$grammar.Name] = $true
    Copy-Item $grammar.FullName (Join-Path $Destination $grammar.Name) -Force
}
Write-Host "installed $($seen.Count) grammar libraries"
$onPath = ($env:PATH -split ";") -contains $Destination
if (-not $onPath) {
    Write-Host "Add $Destination to PATH to run next_code from any shell, e.g.:"
    Write-Host "  [Environment]::SetEnvironmentVariable('PATH', `$env:PATH + ';$Destination', 'User')"
}
