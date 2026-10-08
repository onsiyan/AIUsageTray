[CmdletBinding()]
param(
    # Where the setup is written; target\dist by default.
    [string]$OutputDirectory,
    # Skip the test run, for a quick local package.
    [switch]$SkipTests
)

# Tests and builds the release, then packages it with Inno Setup 6
# (winget install JRSoftware.InnoSetup) into UsageMonitor-<version>-Setup.exe.

$ErrorActionPreference = 'Stop'
$rustRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\..\..'))
$targetDirectory = Join-Path $rustRoot 'target'
$releaseDirectory = Join-Path $targetDirectory 'release'
$workspaceManifest = Join-Path $rustRoot 'Cargo.toml'
$setupScript = Join-Path $PSScriptRoot 'UsageMonitor.iss'
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $targetDirectory 'dist'
}
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)

$version = Select-String -LiteralPath $workspaceManifest -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1 |
    ForEach-Object { $_.Matches[0].Groups[1].Value }
if (-not $version) {
    throw "Could not read the workspace version from '$workspaceManifest'."
}

$compiler = @(
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } | Select-Object -First 1
if (-not $compiler) {
    throw 'Inno Setup 6 is not installed. Install it with: winget install JRSoftware.InnoSetup'
}

$setupPath = Join-Path $OutputDirectory "UsageMonitor-$version-Setup.exe"
if (Test-Path -LiteralPath $setupPath) {
    throw "Refusing to overwrite an existing installer: $setupPath"
}

function Invoke-Cargo([string[]]$Arguments) {
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
    }
}

$cargoArguments = @('--workspace', '--release', '--locked', '--manifest-path', $workspaceManifest, '--target-dir', $targetDirectory)
if (-not $SkipTests) {
    Invoke-Cargo (@('test') + $cargoArguments)
}
Invoke-Cargo (@('build') + $cargoArguments)

foreach ($file in 'usage-monitor.exe', 'usage-monitor-cli.exe', 'usage-monitor-login.exe') {
    if (-not (Test-Path -LiteralPath (Join-Path $releaseDirectory $file) -PathType Leaf)) {
        throw "Release build did not produce required package file '$file'."
    }
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
& $compiler /Qp "/DAppVersion=$version" "/DReleaseDir=$releaseDirectory" "/DOutputDir=$OutputDirectory" $setupScript
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $setupPath -PathType Leaf)) {
    throw "Inno Setup did not create the installer (exit code $LASTEXITCODE)."
}

$installer = Get-Item -LiteralPath $setupPath
Write-Output ("Created {0} ({1:N0} bytes)" -f $installer.FullName, $installer.Length)
Get-FileHash -LiteralPath $setupPath -Algorithm SHA256 | Select-Object Algorithm, Hash, Path
