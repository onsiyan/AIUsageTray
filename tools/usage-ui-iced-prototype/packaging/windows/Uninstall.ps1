$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms

$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\UsageMonitorPreview'
$appPath = Join-Path $installRoot 'usage-ui-iced-prototype.exe'
$runningApp = Get-Process -Name 'usage-ui-iced-prototype' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $appPath } |
    Select-Object -First 1
if ($runningApp) {
    [System.Windows.Forms.MessageBox]::Show(
        'Close Usage Monitor Preview and run uninstall again.',
        'Usage Monitor Preview Uninstall',
        [System.Windows.Forms.MessageBoxButtons]::OK,
        [System.Windows.Forms.MessageBoxIcon]::Information
    ) | Out-Null
    exit 1
}

$files = @(
    'usage-ui-iced-prototype.exe',
    'codex-usage.exe',
    'codex-usage-codex-probe.exe',
    'codex-usage-claude-probe.exe',
    'codex-usage-oauth-probe.exe',
    'codex-usage-opencode-go-probe.exe',
    'codex-usage-openrouter-probe.exe',
    'Uninstall.ps1'
)
foreach ($file in $files) {
    $path = Join-Path $installRoot $file
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        Remove-Item -LiteralPath $path -Force
    }
}

$shortcutFolder = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Usage Monitor Preview'
if (Test-Path -LiteralPath $shortcutFolder -PathType Container) {
    Remove-Item -LiteralPath $shortcutFolder -Recurse -Force
}
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\UsageMonitorPreview'
if (Test-Path -LiteralPath $uninstallKey) {
    Remove-Item -LiteralPath $uninstallKey -Recurse -Force
}
if ((Test-Path -LiteralPath $installRoot -PathType Container) -and -not (Get-ChildItem -LiteralPath $installRoot -Force | Select-Object -First 1)) {
    Remove-Item -LiteralPath $installRoot -Force
}

[System.Windows.Forms.MessageBox]::Show(
    'Usage Monitor Preview was removed. Account data and sign-in credentials were left untouched.',
    'Usage Monitor Preview Uninstall',
    [System.Windows.Forms.MessageBoxButtons]::OK,
    [System.Windows.Forms.MessageBoxIcon]::Information
) | Out-Null
