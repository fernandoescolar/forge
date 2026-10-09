# Installs or updates Forge on Windows from its latest GitHub release:
#
#   irm https://raw.githubusercontent.com/fernandoescolar/forge/main/scripts/install.ps1 | iex
#
# It downloads Forge-<version>-windows-<arch>.zip, checks its SHA-256 against the release's,
# puts it in %LOCALAPPDATA%\Programs\Forge, adds Forge to the Start menu and a `forge` command
# to your PATH (`forge .` opens the current folder, in the Forge that is running if there is
# one). A Forge that is running keeps running: restart it to use the new one.
#
#   $env:FORGE_VERSION = '0.0.2'          install that version instead of the latest
#   $env:FORGE_INSTALL_DIR = '<dir>'      where the forge folder goes
#   $env:FORGE_REPOSITORY = 'owner/repo'
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Say([string]$Message) { Write-Host "==> $Message" -ForegroundColor White }

$Repo = if ($env:FORGE_REPOSITORY) { $env:FORGE_REPOSITORY } else { 'fernandoescolar/forge' }
$Arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$Headers = @{ 'User-Agent' = 'forge-install' }

if ($env:FORGE_VERSION) {
    $Tag = 'v' + $env:FORGE_VERSION.TrimStart('v')
    $Release = Invoke-RestMethod -Headers $Headers "https://api.github.com/repos/$Repo/releases/tags/$Tag"
} else {
    Say "Looking for the latest release of $Repo"
    $Release = Invoke-RestMethod -Headers $Headers "https://api.github.com/repos/$Repo/releases/latest"
    $Tag = $Release.tag_name
}
$Version = $Tag.TrimStart('v')
$ZipName = "Forge-$Version-windows-$Arch.zip"
$Asset = $Release.assets | Where-Object { $_.name -eq $ZipName } | Select-Object -First 1
if (-not $Asset) { throw "$Repo's $Tag release has no $ZipName" }
if (-not $Asset.digest) { throw "the release doesn't list $ZipName's SHA-256" }

$Temp = Join-Path ([System.IO.Path]::GetTempPath()) ("forge-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $Temp | Out-Null
try {
    Say "Downloading Forge $Version for Windows ($Arch)"
    $Zip = Join-Path $Temp $ZipName
    Invoke-WebRequest -Headers $Headers -Uri $Asset.browser_download_url -OutFile $Zip
    $Actual = 'sha256:' + (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLower()
    if ($Actual -ne $Asset.digest.ToLower()) { throw "$ZipName's SHA-256 isn't the one the release lists" }

    Expand-Archive -Path $Zip -DestinationPath (Join-Path $Temp 'unpacked')
    $New = Join-Path $Temp 'unpacked\forge'
    if (-not (Test-Path (Join-Path $New 'bin\forge.exe'))) { throw "$ZipName isn't a Forge zip" }

    $InstallDir = if ($env:FORGE_INSTALL_DIR) { $env:FORGE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\Forge' }
    $Prefix = Join-Path $InstallDir 'forge'
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Say "Installing $Prefix"
    # A running forge.exe can't be overwritten or moved with its folder, but it can be
    # renamed: put each new file in place and leave the replaced ones as *.forge-old, which
    # Forge removes when it next starts (as its updater does).
    Get-ChildItem -Recurse -File $New | ForEach-Object {
        $Relative = $_.FullName.Substring($New.Length + 1)
        $Target = Join-Path $Prefix $Relative
        New-Item -ItemType Directory -Force -Path (Split-Path $Target) | Out-Null
        if (Test-Path $Target) {
            $Old = "$Target.forge-old"
            if (Test-Path $Old) { Remove-Item -Force $Old -ErrorAction SilentlyContinue }
            Rename-Item -Path $Target -NewName (Split-Path -Leaf $Old)
        }
        Move-Item -Path $_.FullName -Destination $Target
    }
    $Exe = Join-Path $Prefix 'bin\forge.exe'

    # The Start menu.
    $Shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Forge.lnk'
    $Shell = New-Object -ComObject WScript.Shell
    $Link = $Shell.CreateShortcut($Shortcut)
    $Link.TargetPath = $Exe
    $Link.IconLocation = "$Exe,0"
    $Link.Description = 'Forge'
    $Link.Save()

    # `forge [paths…]`: starts Forge without holding the terminal (or hands the paths to the
    # running one).
    $CliDir = Join-Path $Prefix 'cli'
    New-Item -ItemType Directory -Force -Path $CliDir | Out-Null
    Set-Content -Encoding ascii -Path (Join-Path $CliDir 'forge.cmd') -Value "@echo off`r`nstart `"`" `"%~dp0..\bin\forge.exe`" %*"
    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($UserPath -split ';') -contains $CliDir)) {
        [Environment]::SetEnvironmentVariable('Path', ($(if ($UserPath) { "$UserPath;" } else { '' }) + $CliDir), 'User')
        Say "Added $CliDir to your PATH: open a new terminal to use the forge command."
    }

    if (Get-Process -Name forge -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$Prefix*" }) {
        Say "Forge $Version is installed. Restart Forge to use it."
    } else {
        Say "Forge $Version is installed. Open it from the Start menu, or run: forge ."
    }
} finally {
    Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
}
