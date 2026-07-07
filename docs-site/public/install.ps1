#
# Nebula Installer for Windows
# https://flux159.github.io/nebula/
#
# Usage: irm https://flux159.github.io/nebula/install.ps1 | iex
#
# Options (environment variables):
#   NEBULA_INSTALL_DIR  Engine install directory (default: $HOME\.nebula\engine)
#   NEBULA_VERSION      Release tag to install (default: the latest release,
#                       e.g. v0.1.0)
#
# Installs nebula.exe, nebulad.exe, and krun.dll, and adds them to your PATH.
# Requires Windows with Hyper-V / the Windows Hypervisor Platform (no WSL2).
#

$ErrorActionPreference = "Stop"

$Repo = "Flux159/nebula"
$InstallDir = if ($env:NEBULA_INSTALL_DIR) { $env:NEBULA_INSTALL_DIR } else { "$HOME\.nebula\engine" }

function Write-Banner {
    Write-Host ""
    Write-Host "  _   _      _           _       " -ForegroundColor Blue
    Write-Host " | \ | | ___| |__  _   _| | __ _ " -ForegroundColor Blue
    Write-Host " |  \| |/ _ \ '_ \| | | | |/ _`` |" -ForegroundColor Blue
    Write-Host " | |\  |  __/ |_) | |_| | | (_| |" -ForegroundColor Blue
    Write-Host " |_| \_|\___|_.__/ \__,_|_|\__,_|" -ForegroundColor Blue
    Write-Host ""
}

function Write-Info($msg) {
    Write-Host "==> " -NoNewline -ForegroundColor Blue
    Write-Host $msg
}

function Write-Success($msg) {
    Write-Host "==> " -NoNewline -ForegroundColor Green
    Write-Host $msg
}

function Write-Warn($msg) {
    Write-Host "Warning: " -NoNewline -ForegroundColor Yellow
    Write-Host $msg
}

function Write-Error-Exit($msg) {
    Write-Host "Error: " -NoNewline -ForegroundColor Red
    Write-Host $msg
    exit 1
}

function Get-ReleaseTag {
    if ($env:NEBULA_VERSION) {
        $tag = $env:NEBULA_VERSION
        if ($tag -notlike "v*") { $tag = "v$tag" }
        Write-Info "Using pinned version: $tag"
        return $tag
    }
    Write-Info "Fetching latest release..."
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "NebulaInstaller" }
        if ($release.tag_name) {
            Write-Info "Latest version: $($release.tag_name)"
            return $release.tag_name
        }
    } catch {
        # Fallback: the gh CLI (works while the repository is private).
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            $tag = (gh release view --repo $Repo --json tagName -q .tagName 2>$null)
            if ($tag) {
                Write-Info "Latest version: $tag"
                return $tag
            }
        }
    }
    Write-Error-Exit "Failed to fetch the latest version. Check your internet connection."
}

function Install-Nebula {
    Write-Banner

    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -ne "AMD64") {
        Write-Error-Exit "Nebula on Windows currently supports x64 only (detected: $arch)."
    }
    Write-Info "Detected platform: windows-x86_64"

    $tag = Get-ReleaseTag
    $ver = $tag.TrimStart("v")
    $stage = "nebula-$ver-windows-x86_64"
    $downloadUrl = "https://github.com/$Repo/releases/download/$tag/$stage.zip"

    # Download
    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "nebula-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
    $zipFile = Join-Path $tmpDir "$stage.zip"

    Write-Info "Downloading $stage.zip..."
    $downloaded = $false
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $zipFile -UseBasicParsing
        $downloaded = $true
    } catch {
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            Write-Warn "Direct download failed; retrying with the GitHub CLI..."
            gh release download $tag --repo $Repo --pattern "$stage.zip" --output $zipFile
            if (Test-Path $zipFile) { $downloaded = $true }
        }
    }
    if (-not $downloaded) {
        Write-Error-Exit "Download failed. Check that the release exists: $downloadUrl"
    }

    # Verify checksum when the sidecar file is available
    try {
        $shaUrl = "$downloadUrl.sha256"
        $shaFile = "$zipFile.sha256"
        Invoke-WebRequest -Uri $shaUrl -OutFile $shaFile -UseBasicParsing
        $expected = ((Get-Content $shaFile) -split '\s+')[0].ToLower()
        $actual = (Get-FileHash $zipFile -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            Write-Error-Exit "Checksum verification failed."
        }
        Write-Info "SHA-256 checksum verified"
    } catch {
        Write-Warn "Checksum file not available; skipping verification."
    }

    # Extract — the zip contains a $stage\ folder with nebula.exe, nebulad.exe, krun.dll
    Write-Info "Installing to $InstallDir..."
    Expand-Archive -Path $zipFile -DestinationPath $tmpDir -Force
    if (Test-Path $InstallDir) {
        Remove-Item -Path $InstallDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path (Split-Path $InstallDir) -Force | Out-Null
    $nestedDir = Join-Path $tmpDir $stage
    if (Test-Path $nestedDir) {
        Move-Item -Path $nestedDir -Destination $InstallDir -Force
    } else {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        Get-ChildItem -Path $tmpDir -Exclude "*.zip", "*.sha256" | Move-Item -Destination $InstallDir -Force
    }
    Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue

    Write-Success "Installed to $InstallDir"

    # Add to PATH (the engine dir itself — krun.dll must sit next to the exes)
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($currentPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$currentPath;$InstallDir", "User")
        Write-Info "Added $InstallDir to user PATH"
        # Also update current session
        $env:Path = "$env:Path;$InstallDir"
    } else {
        Write-Info "PATH already configured"
    }

    # Verify
    $nebulaExe = Join-Path $InstallDir "nebula.exe"
    if (Test-Path $nebulaExe) {
        Write-Success "Installation complete!"
        Write-Host ""
        Write-Host "  The PATH has been updated. Restart your terminal, then run:" -ForegroundColor White
        Write-Host ""
        Write-Host "    nebula up" -ForegroundColor Blue
        Write-Host "    nebula setup docker" -ForegroundColor Blue
        Write-Host "    docker run -d -p 8080:80 nginx" -ForegroundColor Blue
        Write-Host ""
        Write-Host "  On first 'nebula up', the guest kernel + rootfs are downloaded" -ForegroundColor White
        Write-Host "  and verified automatically." -ForegroundColor White
        Write-Host ""
        Write-Host "  Documentation: https://flux159.github.io/nebula/" -ForegroundColor Blue
        Write-Host ""
    } else {
        Write-Error-Exit "Installation failed. nebula.exe not found."
    }
}

Install-Nebula
