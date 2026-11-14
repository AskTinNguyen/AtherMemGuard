# Builds a release of Ather MemGuard and packages it for the team.
#
#   .\package.ps1            -> dist\ with the setup exe, a portable zip, checksums and latest.json
#   .\package.ps1 -Publish   -> same, then puts them on the GitHub Release v<version> (created, or
#                               updated with --clobber for a new build of the same version) and
#                               copies latest.json to downloads\latest.json. Commit and push that too.
#
# Running copies find the update the next time they check (the tray's "Check for updates" checks now).
# Set UPDATE_NOTES to one line about what's new; the app shows it when it offers the update.
# RELEASE_NOTES.md is the release page text: update it for every release.
# The version comes from Cargo.toml. To release again under the same version, raise it instead:
# a running copy only updates to a higher version number.
#
# Optional code signing (removes the SmartScreen "unknown publisher" warning):
#   $env:SIGN_PFX = 'path\to\cert.pfx'; $env:SIGN_PASSWORD = '...'   before running.
# The CLI is signed before it's embedded, then the setup exe is signed.

param([switch]$Publish)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$repo = 'AskTinNguyen/AtherMemGuard'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}

function Invoke-Native([string]$exe, [string[]]$arguments) {
    & $exe @arguments
    if ($LASTEXITCODE -ne 0) { throw "$exe $($arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

function Invoke-Sign([string]$file) {
    if (-not $env:SIGN_PFX) { return }
    Invoke-Native 'signtool' @('sign', '/f', $env:SIGN_PFX, '/p', $env:SIGN_PASSWORD, '/fd', 'SHA256',
        '/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', $file)
}

# The version, from the [package] table of Cargo.toml.
$ver = $null
$inPackage = $false
foreach ($line in Get-Content Cargo.toml) {
    if ($line -match '^\s*\[(.+)\]\s*$') { $inPackage = $Matches[1] -eq 'package'; continue }
    if ($inPackage -and $line -match '^\s*version\s*=\s*"([0-9.]+)"') { $ver = $Matches[1]; break }
}
if (-not $ver) { throw 'Could not read the version from Cargo.toml' }

# 1) The CLI on its own, then the GUI exe carrying it (feature embed-cli). The CLI is copied out of
#    target\ first: building the GUI would otherwise be reading a file cargo may rewrite.
Invoke-Native 'cargo' @('build', '--release', '--locked', '--bin', 'memguard')
$embedDir = Join-Path $PSScriptRoot 'target\embed'
New-Item -ItemType Directory -Force $embedDir | Out-Null
$cli = Join-Path $embedDir 'memguard.exe'
Copy-Item -Force 'target\release\memguard.exe' $cli
Invoke-Sign $cli
$env:MEMGUARD_CLI = $cli
try {
    Invoke-Native 'cargo' @('build', '--release', '--locked', '--features', 'embed-cli', '--bin', 'AtherMemGuard')
} finally {
    Remove-Item Env:\MEMGUARD_CLI -ErrorAction SilentlyContinue
}
$gui = 'target\release\AtherMemGuard.exe'
Invoke-Sign $gui

# 2) dist\: the one file teammates need, a portable zip, checksums.
$dist = Join-Path $PSScriptRoot 'dist'
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory $dist | Out-Null
$setupName = "AtherMemGuard-Setup-$ver.exe"
$setup = Join-Path $dist $setupName
Copy-Item $gui $setup

$stage = Join-Path $dist "AtherMemGuard-$ver"
New-Item -ItemType Directory $stage | Out-Null
Copy-Item $gui (Join-Path $stage 'AtherMemGuard.exe')
Copy-Item $cli (Join-Path $stage 'memguard.exe')
Set-Content -Encoding ascii (Join-Path $stage 'AtherMemGuard.portable') 'Run AtherMemGuard.exe from this folder without installing. Delete this file to have it offer to install instead.'
Copy-Item README.md, INSTALL.md $stage
$zip = Join-Path $dist "AtherMemGuard-$ver-portable.zip"
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
Remove-Item -Recurse -Force $stage
Copy-Item INSTALL.md $dist

Get-ChildItem $dist -File | Where-Object { $_.Extension -in '.exe', '.zip' } | Sort-Object Name | ForEach-Object {
    (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower() + '  ' + $_.Name
} | Set-Content -Encoding ascii (Join-Path $dist 'SHA256SUMS.txt')

# 3) What running copies read to find this version (src\updater.rs). Other platforms' entries
#    already in downloads\latest.json are kept.
$f = Get-Item $setup
$windows = [ordered]@{
    version = $ver
    url     = "https://github.com/$repo/releases/download/v$ver/$setupName"
    sha256  = (Get-FileHash $f.FullName -Algorithm SHA256).Hash.ToLower()
    size    = $f.Length
    notes   = [string]$env:UPDATE_NOTES
}
$manifest = [ordered]@{}
if (Test-Path 'downloads\latest.json') {
    foreach ($p in (Get-Content 'downloads\latest.json' -Raw -Encoding UTF8 | ConvertFrom-Json).PSObject.Properties) {
        $manifest[$p.Name] = $p.Value
    }
}
$manifest['windows'] = $windows
[IO.File]::WriteAllText((Join-Path $dist 'latest.json'), (ConvertTo-Json $manifest -Depth 4))

Write-Host ''
Write-Host "Packaged Ather MemGuard $ver$(if ($env:SIGN_PFX) { ' (signed)' }):"
Get-ChildItem $dist | ForEach-Object { Write-Host ('  {0,-40} {1,12:N0} bytes' -f $_.Name, $_.Length) }

if (-not $Publish) { return }

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw 'gh CLI not found' }
# The release is tagged at the commit that was built, so it has to be on GitHub already.
$commit = (git rev-parse HEAD).Trim()
Invoke-Native 'git' @('fetch', '-q', 'origin')
$onRemote = git branch -r --contains $commit | Select-String 'origin/'
if (-not $onRemote) { throw "Push $commit first: the release is tagged there." }
$assets = @($setup, $zip, (Join-Path $dist 'SHA256SUMS.txt'), (Join-Path $dist 'latest.json'))
# "release not found" on stderr is the normal answer for a first release; with
# ErrorActionPreference = Stop, Windows PowerShell would turn it into a fatal error.
$ErrorActionPreference = 'Continue'
& gh release view "v$ver" --repo $repo 2>$null | Out-Null
$releaseExists = ($LASTEXITCODE -eq 0)
$ErrorActionPreference = 'Stop'
if (-not $releaseExists) {
    Invoke-Native 'gh' (@('release', 'create', "v$ver") + $assets + @('--repo', $repo, '--target', $commit,
        '--title', "Ather MemGuard $ver", '--notes-file', 'RELEASE_NOTES.md', '--latest'))
} else {
    Invoke-Native 'gh' (@('release', 'upload', "v$ver") + $assets + @('--repo', $repo, '--clobber'))
    Invoke-Native 'gh' @('release', 'edit', "v$ver", '--repo', $repo, '--notes-file', 'RELEASE_NOTES.md')
}
New-Item -ItemType Directory -Force 'downloads' | Out-Null
Copy-Item -Force (Join-Path $dist 'latest.json') 'downloads\latest.json'
Write-Host "Published v$ver. Commit and push downloads\latest.json too."
