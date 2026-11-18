#Requires -RunAsAdministrator
[CmdletBinding(DefaultParameterSetName='Install')]
param(
    [Parameter(Mandatory, ParameterSetName='Install')][string] $PackageDirectory,
    [Parameter(Mandatory, ParameterSetName='Remove')][ValidatePattern('^oem[0-9]+\.inf$')][string] $RemovePublishedInf
)
$ErrorActionPreference = 'Stop'
if (-not ('AstraRoute.Installation' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace AstraRoute {
    public static class Installation {
        [DllImport("newdev.dll", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
        public static extern bool DiInstallDriverW(IntPtr parent, string path, uint flags, out bool reboot);
        [DllImport("newdev.dll", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
        public static extern bool DiUninstallDriverW(IntPtr parent, string path, uint flags, out bool reboot);
    }
}
'@
}
$reboot = $false

if ($PSCmdlet.ParameterSetName -eq 'Install') {
    foreach ($file in 'AstraRoute.inf', 'AstraRoute.cat', 'AstraRoute.sys') {
        if (!(Test-Path -LiteralPath (Join-Path $PackageDirectory $file) -PathType Leaf)) {
            throw "Driver package is missing $file"
        }
    }
    $inf = (Resolve-Path -LiteralPath (Join-Path $PackageDirectory 'AstraRoute.inf')).Path
    if (![AstraRoute.Installation]::DiInstallDriverW([IntPtr]::Zero, $inf, 0, [ref] $reboot)) {
        throw [ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error())
    }
    if ($reboot) { Write-Output 'Restart Windows to finish installing AstraRoute.'; return }
    $driver = Get-CimInstance Win32_SystemDriver -Filter "Name='AstraRoute'"
    if (!$driver) { throw 'Windows staged the package but has not registered the AstraRoute service.' }
    if ($driver.State -ne 'Running') {
        & sc.exe start AstraRoute
        if ($LASTEXITCODE -ne 0) { throw "Starting AstraRoute failed with $LASTEXITCODE" }
    }
} else {
    $package = Get-WindowsDriver -Online -Driver $RemovePublishedInf
    if ($package.ProviderName -ne 'Astra' -or [IO.Path]::GetFileName($package.OriginalFileName) -ine 'AstraRoute.inf') {
        throw "$RemovePublishedInf does not identify an AstraRoute package."
    }
    $driver = Get-CimInstance Win32_SystemDriver -Filter "Name='AstraRoute'"
    if ($driver -and $driver.State -ne 'Stopped') {
        & sc.exe stop AstraRoute
        if ($LASTEXITCODE -ne 0) { throw "Stopping AstraRoute failed with $LASTEXITCODE" }
    }
    if (![AstraRoute.Installation]::DiUninstallDriverW([IntPtr]::Zero, $package.OriginalFileName, 0, [ref] $reboot)) {
        throw [ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error())
    }
    if ($reboot) { Write-Output 'Restart Windows to finish removing AstraRoute.' }
}
