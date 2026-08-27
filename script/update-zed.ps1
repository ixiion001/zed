<#
.SYNOPSIS
    Updates this fork's patched Zed build from its GitHub releases.

.DESCRIPTION
    These builds are dev-channel deliberately. A stable-channel binary would let
    Zed's own updater download official Zed over the top and silently discard the
    Claude Code IDE patch, so that updater is off — and this takes its place.

    Reads /releases/latest, which GitHub defines to exclude prereleases. That
    exclusion is the entire promotion gate: the weekly rebase publishes a
    prerelease, and no machine moves until someone promotes it by hand. -Pre opts
    in for testing.

.PARAMETER Pre
    Consider prereleases too. Use this to try a build before promoting it.

.PARAMETER DryRun
    Report what would happen and change nothing.

.EXAMPLE
    .\update-zed.ps1 -DryRun
    .\update-zed.ps1 -Pre
#>
[CmdletBinding()]
param(
    [switch] $Pre,
    [switch] $DryRun,
    [string] $InstallDir = "$env:LOCALAPPDATA\Programs\ZedDev",
    [string] $Repo = 'ixiion001/zed'
)

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest is roughly an order of magnitude slower with the progress bar
# drawn, which matters for a 140 MB asset.
$ProgressPreference = 'SilentlyContinue'

$api = "https://api.github.com/repos/$Repo"
$headers = @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'update-zed' }
$exe = Join-Path $InstallDir 'zed.exe'
$assetName = 'zed-claude-code-windows-x64.zip'

# Get-FileHash is absent from some locked-down Windows PowerShell installs, so
# hash through .NET rather than depend on it.
function Get-Sha256([string] $Path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($Path)
    try { ([BitConverter]::ToString($sha.ComputeHash($stream)) -replace '-', '').ToLower() }
    finally { $stream.Dispose(); $sha.Dispose() }
}

# Two identical-looking zed.exe files on one machine is a trap: a local build tree
# and this install are both just "Zed" in the Start Menu, and launching the wrong
# one looks exactly like an update that did not apply. Give this one a name that
# says what it is.
function Set-StartMenuShortcut {
    $startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
    $link = Join-Path $startMenu 'Zed (Claude Code).lnk'
    try {
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut($link)
        if ($shortcut.TargetPath -eq $exe) { return }
        $shortcut.TargetPath = $exe
        $shortcut.WorkingDirectory = $InstallDir
        $shortcut.Description = 'Zed with Claude Code IDE integration (unofficial build)'
        $shortcut.Save()
        Write-Host "start menu  : Zed (Claude Code)"
    } catch {
        # Not worth failing an otherwise good update over.
        Write-Warning "could not create the Start Menu shortcut: $($_.Exception.Message)"
    }
}

# The build stamps its commit into the version, e.g. 1.16.3+dev.2.<40 hex chars>.
# Comparing that against the release's commit keeps this stateless: it stays
# correct even for an install someone unzipped by hand.
function Get-InstalledCommit {
    if (-not (Test-Path $exe)) { return $null }
    $version = (Get-Item $exe).VersionInfo.ProductVersion
    if ($version -match '([0-9a-f]{40})$') { return $Matches[1] }
    return $null
}

$installed = Get-InstalledCommit

# Sweep up the binary a previous run moved aside. This happens before deciding
# whether an update is due, because otherwise a 418 MB leftover would sit there
# until the *next* update rather than the next run. Anything still open stays
# put and gets collected later.
if (-not $DryRun) {
    Get-ChildItem $InstallDir -Filter '*.old-*' -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
}

Write-Host "install dir : $InstallDir"
Write-Host "installed   : $(if ($installed) { $installed.Substring(0, 10) } else { 'nothing found' })"

if ($Pre) {
    $release = Invoke-RestMethod "$api/releases?per_page=10" -Headers $headers |
        Where-Object { -not $_.draft } | Select-Object -First 1
} else {
    try {
        $release = Invoke-RestMethod "$api/releases/latest" -Headers $headers
    } catch {
        # 404 here is the normal state while only prereleases exist: nothing has
        # been promoted, so there is nothing to update to. The exception type
        # differs between Windows PowerShell and PowerShell 7, so match on the
        # status code rather than the type.
        $status = $_.Exception.Response.StatusCode
        if ($null -eq $status -or [int] $status -ne 404) { throw }
        $release = $null
    }
}

if (-not $release) {
    Write-Host 'latest      : none promoted yet'
    Write-Host 'up to date. Re-run with -Pre to try an unpromoted prerelease.'
    exit 0
}

$commit = (Invoke-RestMethod "$api/commits/$($release.tag_name)" -Headers $headers).sha
$label = if ($release.prerelease) { "$($release.tag_name) (prerelease)" } else { $release.tag_name }
Write-Host "latest      : $label  $($commit.Substring(0, 10))"

if ($commit -eq $installed) {
    if (-not $DryRun) { Set-StartMenuShortcut }
    Write-Host 'up to date.'
    exit 0
}

$asset = $release.assets | Where-Object name -EQ $assetName
$sumAsset = $release.assets | Where-Object name -EQ "$assetName.sha256"
if (-not $asset -or -not $sumAsset) {
    throw "release $($release.tag_name) has no $assetName and matching .sha256"
}

$sizeMb = [math]::Round($asset.size / 1MB)
$from = if ($installed) { $installed.Substring(0, 10) } else { 'fresh install' }
if ($DryRun) {
    Write-Host "would update: $from -> $($commit.Substring(0, 10))  ($sizeMb MB)"
    exit 0
}

$temp = Join-Path ([System.IO.Path]::GetTempPath()) "update-zed-$($release.tag_name)"
New-Item -ItemType Directory -Force -Path $temp | Out-Null
$zip = Join-Path $temp $assetName

Write-Host "downloading : $sizeMb MB"
Invoke-WebRequest $asset.browser_download_url -Headers $headers -OutFile $zip

# Fetched to a file rather than read from .Content: GitHub serves the checksum as
# application/octet-stream, which Windows PowerShell hands back as a byte array.
$sumFile = "$zip.sha256"
Invoke-WebRequest $sumAsset.browser_download_url -Headers $headers -OutFile $sumFile
$expected = ((Get-Content $sumFile -Raw) -split '\s+')[0]
$actual = Get-Sha256 $zip
if ($expected -ne $actual) {
    throw "checksum mismatch: expected $expected, got $actual. Refusing to install."
}
Write-Host 'checksum    : OK'

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

# Deliberately not `Expand-Archive -DestinationPath $InstallDir -Force`: that
# deletes each existing file before writing its replacement, and dies on
# anything the running editor holds open. OpenConsole.exe is a live process
# whenever a terminal is open, and conpty.dll is loaded on demand — so the
# failure is intermittent, which is the worst kind. It also fails *after*
# removing earlier entries, leaving the install half-updated and unlaunchable.
#
# Stage the archive instead and move files in one at a time.
$staging = Join-Path $temp 'staging'
Expand-Archive $zip -DestinationPath $staging -Force

$stamp = if ($installed) { $installed.Substring(0, 10) } else { 'unknown' }
foreach ($file in Get-ChildItem $staging -File) {
    $target = Join-Path $InstallDir $file.Name
    # conpty.dll and OpenConsole.exe come from a pinned download and are usually
    # identical between builds. Skipping them means the common update never has
    # to fight a lock at all.
    if ((Test-Path $target) -and (Get-Sha256 $target) -eq (Get-Sha256 $file.FullName)) {
        continue
    }
    if (Test-Path $target) {
        # A file that is open cannot be deleted or overwritten, but renaming one
        # is usually permitted — that is how the running zed.exe gets replaced.
        try {
            Rename-Item $target "$($file.Name).old-$stamp" -Force -ErrorAction Stop
        } catch {
            throw "cannot replace $($file.Name): it is in use. Close Zed and run this again."
        }
    }
    Move-Item $file.FullName $target -Force
}

Remove-Item $temp -Recurse -Force

Set-StartMenuShortcut

$now = (Get-Item $exe).VersionInfo.ProductVersion
Write-Host "installed   : $now"
Write-Host "done. Restart Zed from the Start Menu entry, or from $exe"
