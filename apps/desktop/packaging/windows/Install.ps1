$ErrorActionPreference = 'Stop'

function Show-SetupMessage([string]$Message, [string]$Title, [string]$Icon) {
    $iconValue = [System.Enum]::Parse([System.Windows.Forms.MessageBoxIcon], $Icon)
    [System.Windows.Forms.MessageBox]::Show(
        $Message,
        $Title,
        [System.Windows.Forms.MessageBoxButtons]::OK,
        $iconValue
    ) | Out-Null
}

try {
    Add-Type -AssemblyName System.Windows.Forms

    $payloadRoot = $PSScriptRoot
    $installRoot = Join-Path $env:LOCALAPPDATA 'Programs\UsageMonitor'
    $files = @(
        'usage-monitor.exe',
        'usage-monitor-cli.exe',
        'usage-monitor-login.exe'
    )

    foreach ($file in $files) {
        if (-not (Test-Path -LiteralPath (Join-Path $payloadRoot $file) -PathType Leaf)) {
            throw "Installer payload is missing '$file'."
        }
    }

    $appPath = Join-Path $installRoot 'usage-monitor.exe'
    $runningApp = Get-Process -Name 'usage-monitor' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $appPath } |
        Select-Object -First 1
    if ($runningApp) {
        throw 'Close Usage Monitor before installing this update, then run setup again.'
    }

    New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
    foreach ($file in $files) {
        Copy-Item -LiteralPath (Join-Path $payloadRoot $file) -Destination (Join-Path $installRoot $file) -Force
    }
    Copy-Item -LiteralPath (Join-Path $payloadRoot 'Uninstall.ps1') -Destination (Join-Path $installRoot 'Uninstall.ps1') -Force

    $startMenuFolder = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Usage Monitor'
    New-Item -ItemType Directory -Path $startMenuFolder -Force | Out-Null
    $shortcutPath = Join-Path $startMenuFolder 'Usage Monitor.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = $appPath
    $shortcut.WorkingDirectory = $installRoot
    $shortcut.Description = 'Usage Monitor'
    $shortcut.Save()

    $uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\UsageMonitor'
    New-Item -Path $uninstallKey -Force | Out-Null
    $powershellPath = Join-Path $PSHOME 'powershell.exe'
    $uninstallerPath = Join-Path $installRoot 'Uninstall.ps1'
    $uninstallCommand = '"{0}" -NoProfile -ExecutionPolicy Bypass -File "{1}"' -f $powershellPath, $uninstallerPath
    New-ItemProperty -Path $uninstallKey -Name 'DisplayName' -Value 'Usage Monitor' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $uninstallKey -Name 'DisplayVersion' -Value '0.1.0' -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $uninstallKey -Name 'InstallLocation' -Value $installRoot -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $uninstallKey -Name 'UninstallString' -Value $uninstallCommand -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $uninstallKey -Name 'NoModify' -Value 1 -PropertyType DWord -Force | Out-Null
    New-ItemProperty -Path $uninstallKey -Name 'NoRepair' -Value 1 -PropertyType DWord -Force | Out-Null

    Show-SetupMessage "Installed successfully for this Windows user.`n`nOpen it from Start Menu > Usage Monitor.`n`nInstall location: $installRoot" 'Usage Monitor Setup' 'Information'
    exit 0
}
catch {
    try {
        Add-Type -AssemblyName System.Windows.Forms
        Show-SetupMessage $_.Exception.Message 'Usage Monitor Setup failed' 'Error'
    }
    catch {
        Write-Error $_
    }
    exit 1
}
