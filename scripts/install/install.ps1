$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repository = if ($env:EGGDB_REPOSITORY) { $env:EGGDB_REPOSITORY } else { 'eggspot/EggDB' }
$base = if ($env:EGGDB_DOWNLOAD_BASE) { $env:EGGDB_DOWNLOAD_BASE } else { "https://github.com/$repository/releases/download" }
$installDir = if ($env:EGGDB_INSTALL_DIR) { $env:EGGDB_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'EggDB\bin' }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "eggdb install: no release build for the $($env:PROCESSOR_ARCHITECTURE) processor yet" }
}

$headers = @{}
if ($env:GITHUB_TOKEN) { $headers['Authorization'] = "Bearer $env:GITHUB_TOKEN" }

$version = $env:EGGDB_VERSION
if (-not $version) {
    $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$repository/releases/latest" -Headers $headers
    $version = $latest.tag_name
}
$version = $version.TrimStart('v')

$package = "eggdb-v$version-$arch-pc-windows-msvc"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("eggdb-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $archive = Join-Path $work "$package.zip"
    $sums = Join-Path $work 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$base/v$version/$package.zip" -OutFile $archive
    Invoke-WebRequest -UseBasicParsing -Uri "$base/v$version/SHA256SUMS" -OutFile $sums
    $line = Get-Content $sums | Where-Object { $_ -match " $([regex]::Escape("$package.zip"))$" } | Select-Object -First 1
    if (-not $line) { throw "eggdb install: SHA256SUMS lists no $package.zip" }
    $expected = ($line -split ' ')[0]
    $actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($actual -ne $expected.ToLowerInvariant()) { throw "eggdb install: the download's checksum does not match SHA256SUMS" }
    Expand-Archive -Path $archive -DestinationPath $work -Force
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item -Force (Join-Path $work "$package\eggdb.exe") (Join-Path $installDir 'eggdb.exe')
} finally {
    Remove-Item -Recurse -Force $work
}

Write-Output "eggdb $version installed in $installDir"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not ($userPath -split ';' | Where-Object { $_ -eq $installDir })) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$installDir", 'User')
    Write-Output "added $installDir to your PATH; open a new terminal to use eggdb"
}
