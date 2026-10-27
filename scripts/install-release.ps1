# Installs next_code from a GitHub release: the latest, or
# $env:NEXT_CODE_VERSION (a tag such as v0.1.0). x86_64 Windows only. The
# files go to %LOCALAPPDATA%\next_code\bin, which is added to the user PATH,
# with a Start menu shortcut. Running it again updates the installation.
#
#   irm https://raw.githubusercontent.com/AmedeoBiolatti/nextcode/main/scripts/install-release.ps1 | iex
#
# $env:NEXT_CODE_DOWNLOAD_URL replaces the release's download address (a
# mirror, or a local copy of the release files).

# A script block, so that run through `iex` nothing leaks into the session.
& {
    $ErrorActionPreference = "Stop"
    # Windows PowerShell's Invoke-WebRequest is many times slower with its progress bar.
    $ProgressPreference = "SilentlyContinue"
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = "AmedeoBiolatti/nextcode"
    $archive = "next_code-x86_64-windows.zip"
    if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
        throw "No prebuilt next_code for $env:PROCESSOR_ARCHITECTURE; see the README to build it from source."
    }
    $url = "https://github.com/$repo/releases/latest/download"
    if ($env:NEXT_CODE_VERSION) { $url = "https://github.com/$repo/releases/download/$env:NEXT_CODE_VERSION" }
    if ($env:NEXT_CODE_DOWNLOAD_URL) { $url = $env:NEXT_CODE_DOWNLOAD_URL }
    $destination = Join-Path $env:LOCALAPPDATA "next_code\bin"

    $download = Join-Path ([IO.Path]::GetTempPath()) "next_code-$([Guid]::NewGuid())"
    New-Item -ItemType Directory -Path $download | Out-Null
    try {
        $zip = Join-Path $download $archive
        $sums = Join-Path $download "SHA256SUMS"
        Write-Host "Downloading $url/$archive"
        Invoke-WebRequest "$url/$archive" -OutFile $zip -UseBasicParsing
        Invoke-WebRequest "$url/SHA256SUMS" -OutFile $sums -UseBasicParsing
        $expected = $null
        foreach ($line in Get-Content $sums) {
            $hash, $name = $line -split "\s+", 2
            if ($name -eq $archive) { $expected = $hash }
        }
        if (-not $expected -or $expected -ne (Get-FileHash $zip -Algorithm SHA256).Hash) {
            throw "$archive does not match the release's SHA256SUMS"
        }
        Expand-Archive $zip -DestinationPath $download

        New-Item -ItemType Directory -Force -Path $destination | Out-Null
        # Leftovers of earlier updates (see below), unless still in use.
        Get-ChildItem $destination -Filter "*.old-*" |
            Remove-Item -Force -ErrorAction SilentlyContinue
        foreach ($file in Get-ChildItem (Join-Path $download "next_code")) {
            $target = Join-Path $destination $file.Name
            if (Test-Path $target) {
                # A running editor's files cannot be overwritten, but they can
                # be renamed (a resident editor exits once its executable is
                # replaced).
                Rename-Item $target "$($file.Name).old-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
            }
            Copy-Item $file.FullName $target
        }
    } finally {
        Remove-Item -Recurse -Force $download -ErrorAction SilentlyContinue
    }

    # The user PATH, written as the registry keeps it (%VARIABLES% unexpanded).
    $environment = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
    try {
        $path = $environment.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if (($path -split ";") -notcontains $destination) {
            $path = (@($path.TrimEnd(";"), $destination) | Where-Object { $_ }) -join ";"
            $environment.SetValue("Path", $path, [Microsoft.Win32.RegistryValueKind]::ExpandString)
            # Setting any user variable tells running programs (Explorer) that the environment changed.
            [Environment]::SetEnvironmentVariable("NEXT_CODE_INSTALL", $null, "User")
            $env:Path = "$env:Path;$destination"
            Write-Host "Added $destination to your PATH (open a new terminal to use it)"
        }
    } finally {
        $environment.Close()
    }

    $shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut(
        (Join-Path ([Environment]::GetFolderPath("Programs")) "NextCode.lnk"))
    $shortcut.TargetPath = Join-Path $destination "next_code.exe"
    $shortcut.Save()

    Write-Host "Installed next_code in $destination"
    Write-Host "Run next_code, or open NextCode from the Start menu."
}
