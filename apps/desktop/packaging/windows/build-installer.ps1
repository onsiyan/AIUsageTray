[CmdletBinding()]
param(
    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'
$rustRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\..\..'))
$targetDirectory = Join-Path $rustRoot 'target'
$releaseDirectory = Join-Path $targetDirectory 'release'
$workspaceManifest = Join-Path $rustRoot 'Cargo.toml'
$installerSourceDirectory = $PSScriptRoot
$iexpress = Join-Path $env:WINDIR 'System32\iexpress.exe'

if (-not $OutputPath) {
    $OutputPath = Join-Path $releaseDirectory 'UsageMonitor-0.1.0-Setup.exe'
}
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)
if (Test-Path -LiteralPath $OutputPath) {
    throw "Refusing to overwrite an existing installer: $OutputPath"
}
if (-not (Test-Path -LiteralPath $iexpress -PathType Leaf)) {
    throw "Windows IExpress is not available at '$iexpress'."
}

$packageFiles = @(
    'usage-monitor.exe',
    'usage-monitor-cli.exe',
    'usage-monitor-login.exe'
)

function Invoke-Cargo([string[]]$Arguments) {
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
    }
}

Invoke-Cargo @('test', '--workspace', '--release', '--offline', '-j', '2', '--manifest-path', $workspaceManifest, '--target-dir', $targetDirectory)
Invoke-Cargo @('build', '--workspace', '--release', '--offline', '-j', '2', '--manifest-path', $workspaceManifest, '--target-dir', $targetDirectory)

foreach ($file in $packageFiles) {
    if (-not (Test-Path -LiteralPath (Join-Path $releaseDirectory $file) -PathType Leaf)) {
        throw "Release build did not produce required package file '$file'."
    }
}

$outputDirectory = Split-Path -Parent $OutputPath
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
$stageDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ('UsageMonitor-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stageDirectory | Out-Null
$packageCreated = $false

try {
    foreach ($file in $packageFiles) {
        Copy-Item -LiteralPath (Join-Path $releaseDirectory $file) -Destination (Join-Path $stageDirectory $file)
    }
    Copy-Item -LiteralPath (Join-Path $installerSourceDirectory 'Install.ps1') -Destination (Join-Path $stageDirectory 'Install.ps1')
    Copy-Item -LiteralPath (Join-Path $installerSourceDirectory 'Uninstall.ps1') -Destination (Join-Path $stageDirectory 'Uninstall.ps1')

    $sedPath = Join-Path $stageDirectory 'UsageMonitor.sed'
    $sedLines = @(
        '[Version]',
        'Class=IEXPRESS',
        'SEDVersion=3',
        '[Options]',
        'PackagePurpose=InstallApp',
        'ShowInstallProgramWindow=0',
        'HideExtractAnimation=1',
        'UseLongFileName=1',
        'InsideCompressed=1',
        'CAB_FixedSize=0',
        'CAB_ResvCodeSigning=0',
        'RebootMode=N',
        'InstallPrompt=%InstallPrompt%',
        'DisplayLicense=%DisplayLicense%',
        'FinishMessage=%FinishMessage%',
        'TargetName=%TargetName%',
        'FriendlyName=%FriendlyName%',
        'AppLaunched=%AppLaunched%',
        'PostInstallCmd=%PostInstallCmd%',
        'AdminQuietInstCmd=%AdminQuietInstCmd%',
        'UserQuietInstCmd=%UserQuietInstCmd%',
        'SourceFiles=SourceFiles',
        '[Strings]',
        'InstallPrompt=',
        'DisplayLicense=',
        'FinishMessage=Setup finished.',
        "TargetName=$OutputPath",
        'FriendlyName=Usage Monitor 0.1.0',
        'AppLaunched=powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File Install.ps1',
        'PostInstallCmd=<None>',
        'AdminQuietInstCmd=',
        'UserQuietInstCmd='
    )
    for ($index = 0; $index -lt $packageFiles.Count; $index++) {
        $sedLines += ('FILE{0}="{1}"' -f $index, $packageFiles[$index])
    }
    $sedLines += ('FILE{0}="Install.ps1"' -f $packageFiles.Count)
    $sedLines += ('FILE{0}="Uninstall.ps1"' -f ($packageFiles.Count + 1))
    $sedLines += '[SourceFiles]'
    $sedLines += "SourceFiles0=$stageDirectory\"
    $sedLines += '[SourceFiles0]'
    for ($index = 0; $index -lt $packageFiles.Count + 2; $index++) {
        $sedLines += ('%FILE{0}%=' -f $index)
    }
    [System.IO.File]::WriteAllLines($sedPath, $sedLines, [System.Text.Encoding]::Default)

    $process = Start-Process -FilePath $iexpress -ArgumentList @('/N', '/Q', $sedPath) -Wait -PassThru
    if ($process.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $OutputPath -PathType Leaf)) {
        throw "IExpress did not create the installer successfully (exit code $($process.ExitCode)). Staging files retained at '$stageDirectory'."
    }
    $packageCreated = $true
    $installer = Get-Item -LiteralPath $OutputPath
    if ($installer.Length -lt 1MB) {
        throw "The installer is unexpectedly small ($($installer.Length) bytes)."
    }
    Write-Output ("Created {0} ({1:N0} bytes)" -f $installer.FullName, $installer.Length)
    Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256 | Select-Object Algorithm, Hash, Path
}
finally {
    if ($packageCreated -and (Test-Path -LiteralPath $stageDirectory -PathType Container)) {
        Remove-Item -LiteralPath $stageDirectory -Recurse -Force
    }
}
