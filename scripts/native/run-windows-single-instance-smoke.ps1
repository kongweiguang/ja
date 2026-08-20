# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

<#
.SYNOPSIS
  Verify the installed Windows application rejects a second instance and cleans up.

.DESCRIPTION
  This is an engineering smoke only. It exercises the real NSIS installer under a caller-
  selected temporary root (including Unicode and spaces), starts the installed application,
  verifies a second launch exits within a bounded deadline, and then uninstalls the package.
  The evidence remains outside the release gate until signing and ordinary-user acceptance are
  completed.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string] $Installer,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string] $Output,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{7,64}$')]
    [string] $SourceCommit,

    [ValidateRange(10, 600)]
    [int] $TimeoutSeconds = 90,

    [string] $RunnerTemp = '',

    [switch] $RequireUnsigned
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$installerFile = Get-Item -LiteralPath $Installer -Force
if ($installerFile.PSIsContainer -or $installerFile.Extension -ine '.exe') {
    throw "Installer must be an .exe file: $Installer"
}
$outputParent = Split-Path -Parent $Output
if ([string]::IsNullOrWhiteSpace($outputParent)) { $outputParent = (Get-Location).Path }
New-Item -ItemType Directory -Force -Path $outputParent | Out-Null
$outputFile = [IO.FileInfo]::new((Resolve-Path -LiteralPath $outputParent).Path + '\' + (Split-Path -Leaf $Output))
$timeoutMilliseconds = $TimeoutSeconds * 1000
$tempBase = if ([string]::IsNullOrWhiteSpace($RunnerTemp)) {
    if ($env:RUNNER_TEMP -and (Test-Path -LiteralPath $env:RUNNER_TEMP -PathType Container)) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
} else {
    [IO.Path]::GetFullPath($RunnerTemp)
}
New-Item -ItemType Directory -Force -Path $tempBase | Out-Null
$rootPath = Join-Path $tempBase ('ja-single-instance-' + [guid]::NewGuid().ToString('N'))
$installPath = Join-Path $rootPath 'install'
$first = $null
$second = $null
$result = [ordered]@{
    schemaVersion = 1
    product = 'JA'
    sourceCommit = $SourceCommit.ToLowerInvariant()
    engineeringSmokeOnly = $true
    releaseGate = $false
    platform = 'windows'
    architecture = 'x86_64'
    pathClass = if ($tempBase -match '[^\x00-\x7F]' -or $tempBase -match '[ ]') { 'unicode-or-space' } else { 'standard' }
    installer = [ordered]@{
        fileName = $installerFile.Name
        sizeBytes = [int64] $installerFile.Length
        sha256 = (Get-FileHash -LiteralPath $installerFile.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        signingStatus = [string] (Get-AuthenticodeSignature -LiteralPath $installerFile.FullName).Status
    }
    installation = 'pending'
    firstLaunch = 'pending'
    secondLaunch = 'pending'
    secondExitedBeforeDeadline = $false
    secondExitCode = $null
    processTreeReaped = $false
    uninstall = 'pending'
    cleanup = 'pending'
}
$failure = $null

try {
    if ($RequireUnsigned -and $result.installer.signingStatus -ne 'NotSigned') {
        throw "Engineering smoke requires an unsigned installer; actual status: $($result.installer.signingStatus)"
    }
    New-Item -ItemType Directory -Force -Path $rootPath | Out-Null
    $setup = Start-Process -FilePath $installerFile.FullName -ArgumentList @('/S', ('/D=' + $installPath)) -WorkingDirectory $rootPath -PassThru -WindowStyle Hidden
    if (-not $setup.WaitForExit($timeoutMilliseconds)) { throw 'Installer timed out' }
    if ($setup.ExitCode -ne 0) { throw "Installer failed: exit=$($setup.ExitCode)" }
    if (-not (Test-Path -LiteralPath $installPath -PathType Container)) { throw 'Installer did not create install directory' }
    $app = Get-Item -LiteralPath (Join-Path $installPath 'ja.exe') -Force
    $uninstaller = Get-Item -LiteralPath (Join-Path $installPath 'uninstall.exe') -Force
    $sidecars = @(Get-ChildItem -LiteralPath (Join-Path $installPath 'sidecars') -Filter 'ja-agent-*.exe' -File)
    if ($sidecars.Count -ne 1) { throw "Expected one sidecar, found $($sidecars.Count)" }
    $result.installation = 'passed'

    $first = Start-Process -FilePath $app.FullName -WorkingDirectory $installPath -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 1800
    $first.Refresh()
    if ($first.HasExited) { throw "First instance exited early: exit=$($first.ExitCode)" }
    $result.firstLaunch = 'passed'

    $second = Start-Process -FilePath $app.FullName -WorkingDirectory $installPath -PassThru -WindowStyle Hidden
    if (-not $second.WaitForExit(10000)) { throw 'Second instance remained alive past 10 seconds' }
    $second.Refresh()
    $result.secondExitedBeforeDeadline = $true
    $result.secondExitCode = [int] $second.ExitCode
    $result.secondLaunch = 'passed'

    & taskkill.exe /PID $first.Id /T /F 1>$null 2>$null
    if ($LASTEXITCODE -notin @(0, 128)) { throw "First process tree cleanup failed: exit=$LASTEXITCODE" }
    $result.processTreeReaped = $true

    $uninstall = Start-Process -FilePath $uninstaller.FullName -ArgumentList @('/S') -WorkingDirectory $rootPath -PassThru -WindowStyle Hidden
    if (-not $uninstall.WaitForExit($timeoutMilliseconds)) { throw 'Uninstaller timed out' }
    if ($uninstall.ExitCode -ne 0) { throw "Uninstaller failed: exit=$($uninstall.ExitCode)" }
    Start-Sleep -Milliseconds 500
    if (Test-Path -LiteralPath $installPath) { throw 'Install directory remains after uninstall' }
    $result.uninstall = 'passed'
}
catch {
    $failure = $_
    $result.error = $_.Exception.Message
}
finally {
    if ($first -and -not $first.HasExited) { & taskkill.exe /PID $first.Id /T /F 1>$null 2>$null }
    if ($second -and -not $second.HasExited) { & taskkill.exe /PID $second.Id /T /F 1>$null 2>$null }
    if (Test-Path -LiteralPath $rootPath) { Remove-Item -LiteralPath $rootPath -Recurse -Force -ErrorAction SilentlyContinue }
    $result.cleanup = if (Test-Path -LiteralPath $rootPath) { 'failed' } else { 'passed' }
    $json = $result | ConvertTo-Json -Depth 8
    $temporary = [IO.FileInfo]::new($outputFile.FullName + '.tmp')
    [IO.File]::WriteAllText($temporary.FullName, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporary.FullName -Destination $outputFile.FullName -Force
}

$result | ConvertTo-Json -Depth 8
if ($failure -or $result.cleanup -ne 'passed' -or $result.uninstall -ne 'passed') { throw 'Windows single-instance smoke failed' }
