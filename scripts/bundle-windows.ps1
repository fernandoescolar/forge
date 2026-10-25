# Builds dist\forge (a folder to install anywhere) and dist\Forge-<version>-windows-<arch>.zip,
# the file releases and updates use:
#
#   forge\bin\forge.exe                       the app (with its icon)
#   forge\share\forge\extensions\<name>\      the extensions that ship with Forge
#
#   pwsh scripts/bundle-windows.ps1
#
# Needs Rust, Node 20+, and Git for Windows' bash (to build the Database Explorer's sidecar).
# FORGE_UPDATE_REPOSITORY=owner/repo builds a Forge that updates itself from that GitHub
# repository's releases (CI sets it).
$ErrorActionPreference = 'Stop'
$Root = Resolve-Path (Join-Path $PSScriptRoot '..')
Set-Location $Root
$Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$Arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }

function Invoke-Checked([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program $($Arguments -join ' ') failed ($LASTEXITCODE)" }
}

Write-Host '==> cargo build --release'
Invoke-Checked cargo @('build', '--release', '-p', 'forge-native')

# The extensions that ship with Forge (the same as on macOS and Linux).
$Bundled = @('db-explorer', 'containers', 'forge-icons', 'modern-icons', 'colored-icons', 'vscode-great-icons', 'seti-icons')
$Packages = Join-Path ([System.IO.Path]::GetTempPath()) ("forge-packages-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $Packages | Out-Null
Write-Host "==> building and packing the bundled extensions: $($Bundled -join ' ')"
foreach ($name in $Bundled) {
    $ext = Join-Path 'extensions' $name
    $manifest = Get-Content (Join-Path $ext 'package.json') -Raw | ConvertFrom-Json
    # npm runs scripts with cmd.exe, which can't run the sidecar's bash script: run it here.
    if ($manifest.scripts.sidecar) {
        Invoke-Checked bash @("$ext/scripts/build-sidecar.sh", '--host-only')
    }
    Invoke-Checked node @('packages/forge-api/bin/forge-ext.mjs', 'pack', $ext, '-o', (Join-Path $Packages "$name.zip"))
}

$Tree = Join-Path $Root 'dist\forge'
Write-Host "==> assembling $Tree"
if (Test-Path $Tree) { Remove-Item -Recurse -Force $Tree }
New-Item -ItemType Directory -Path (Join-Path $Tree 'bin'), (Join-Path $Tree 'share\forge\extensions') | Out-Null
Copy-Item 'target\release\forge.exe' (Join-Path $Tree 'bin\forge.exe')
foreach ($name in $Bundled) {
    # A .forgeext is a zip (packed here with that extension, which Expand-Archive wants).
    Expand-Archive -Path (Join-Path $Packages "$name.zip") -DestinationPath (Join-Path $Tree "share\forge\extensions\$name")
}
Remove-Item -Recurse -Force $Packages

# Named like Rust's std::env::consts (os and arch), which forge-update looks for.
$Zip = Join-Path $Root "dist\Forge-$Version-windows-$Arch.zip"
Write-Host "==> $Zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
Compress-Archive -Path $Tree -DestinationPath $Zip
$size = '{0:N0} MB' -f ((Get-ChildItem -Recurse $Tree | Measure-Object -Property Length -Sum).Sum / 1MB)
Write-Host "==> done: $Tree ($size)"
