# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

<#
.SYNOPSIS
  Install and launch one unsigned JA NSIS bundle in a private temporary directory.

.DESCRIPTION
  This is an engineering smoke only. It proves that a real Windows NSIS package can install,
  expose the expected app/sidecar files, launch, and uninstall without requiring signing
  credentials. The emitted record explicitly remains outside the release gate.
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

    [switch] $RequireUnsigned
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

<#
.SYNOPSIS
  Compute a relative path on both Windows PowerShell 5.1 and PowerShell 7.

.DESCRIPTION
  Windows PowerShell 5.1 uses .NET Framework, which does not expose Path.GetRelativePath.
  The URI fallback keeps the evidence format stable without requiring a newer shell just for
  path formatting; it never changes which files are inspected or removed.
#>
function Get-PortableRelativePath {
    param(
        [Parameter(Mandatory = $true)][string] $BasePath,
        [Parameter(Mandatory = $true)][string] $ChildPath
    )

    $pathType = [System.IO.Path]
    $relativeMethod = $pathType.GetMethod('GetRelativePath', [Type[]] @([string], [string]))
    if ($null -ne $relativeMethod) {
        return [System.IO.Path]::GetRelativePath($BasePath, $ChildPath)
    }

    $baseFull = [System.IO.Path]::GetFullPath($BasePath).TrimEnd('\') + '\'
    $childFull = [System.IO.Path]::GetFullPath($ChildPath)
    $baseUri = [System.Uri]::new($baseFull)
    $childUri = [System.Uri]::new($childFull)
    return [System.Uri]::UnescapeDataString($baseUri.MakeRelativeUri($childUri).ToString()).Replace('/', '\')
}

<#
.SYNOPSIS
  Return portable file facts and the current Authenticode status for one file.

.DESCRIPTION
  Hashes are collected before uninstall so the report can bind the installed files to the tested
  installer while never serializing a user-specific temporary path or credential-like content.
#>
function Get-FileEvidence {
    param([Parameter(Mandatory = $true)][System.IO.FileInfo] $File)

    if (-not $File.Exists -or $File.Length -le 0) {
        throw "File is missing or empty: $($File.FullName)"
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $File.FullName
    return [ordered]@{
        fileName = $File.Name
        relativePath = $null
        sizeBytes = [int64] $File.Length
        sha256 = (Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        signingStatus = [string] $signature.Status
    }
}

<#
.SYNOPSIS
  Run an installer or uninstaller with a bounded wait and process-tree cleanup.

.DESCRIPTION
  NSIS can otherwise leave a helper process behind when a package is interrupted. The bounded
  process wrapper keeps the smoke deterministic and confines forced cleanup to the process it
  started, which is safer than matching a global executable name.
#>
function Invoke-BoundedProcess {
    param(
        [Parameter(Mandatory = $true)][string] $FilePath,
        [Parameter(Mandatory = $true)][string[]] $ArgumentList,
        [Parameter(Mandatory = $true)][string] $WorkingDirectory,
        [Parameter(Mandatory = $true)][int] $TimeoutMilliseconds
    )

    $process = Start-Process -FilePath $FilePath -ArgumentList $ArgumentList -WorkingDirectory $WorkingDirectory -PassThru -WindowStyle Hidden
    try {
        if (-not $process.WaitForExit($TimeoutMilliseconds)) {
            Stop-ProcessTree -ProcessId $process.Id
            throw "Process timed out: $([System.IO.Path]::GetFileName($FilePath))"
        }
        $process.Refresh()
        return [ordered]@{
            pid = [int] $process.Id
            exitCode = [int] $process.ExitCode
        }
    }
    finally {
        $process.Dispose()
    }
}

<#
.SYNOPSIS
  Terminate only a process tree rooted at a smoke-owned PID.

.DESCRIPTION
  taskkill's `/T` option is the Windows-maintained process-tree primitive. A PID is accepted only
  from a process started by this script, so cleanup does not search for or kill unrelated JA apps.
#>
function Stop-ProcessTree {
    param([Parameter(Mandatory = $true)][int] $ProcessId)

    if ($ProcessId -le 0) { return }
    & taskkill.exe /PID $ProcessId /T /F 1>$null 2>$null
    if ($LASTEXITCODE -notin @(0, 128)) {
        throw "Failed to reap smoke process tree: pid=$ProcessId exit=$LASTEXITCODE"
    }
}

<#
.SYNOPSIS
  Write a JSON evidence document through a same-directory temporary file.

.DESCRIPTION
  Atomic replacement avoids publishing a truncated record if the host is interrupted during
  hashing or cleanup; the path itself is caller-selected and never inferred from the installer.
#>
function Write-Evidence {
    param(
        [Parameter(Mandatory = $true)][System.Collections.IDictionary] $Document,
        [Parameter(Mandatory = $true)][System.IO.FileInfo] $Destination
    )

    $parent = $Destination.Directory
    if ($null -eq $parent) { throw 'Evidence output directory is invalid' }
    $parent.Create()
    $temporary = [System.IO.FileInfo]::new($Destination.FullName + '.tmp')
    $json = $Document | ConvertTo-Json -Depth 8
    [System.IO.File]::WriteAllText($temporary.FullName, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporary.FullName -Destination $Destination.FullName -Force
}

<#
.SYNOPSIS
  Prove that a path remains inside the private smoke root.

.DESCRIPTION
  The installer destination is generated by this run; checking containment before reading or
  deleting files prevents a malformed path argument from turning an installer smoke into broad
  filesystem cleanup.
#>
function Assert-ContainedPath {
    param(
        [Parameter(Mandatory = $true)][System.IO.DirectoryInfo] $Root,
        [Parameter(Mandatory = $true)][string] $Candidate
    )

    $rootPath = $Root.FullName.TrimEnd('\') + '\'
    $candidatePath = ([System.IO.Path]::GetFullPath($Candidate)).TrimEnd('\') + '\'
    if (-not $candidatePath.StartsWith($rootPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Path escaped the smoke temporary root: $Candidate"
    }
}

<#
.SYNOPSIS
  Allow an NSIS self-delete helper a short bounded window to remove installed files.

.DESCRIPTION
  NSIS uninstallers may return before their self-delete helper finishes. Polling only the private
  install directory avoids a timing false negative while preserving a hard upper bound and never
  broadens cleanup beyond this run's generated root.
#>
function Wait-ForInstallFilesRemoved {
    param(
        [Parameter(Mandatory = $true)][string] $InstallDirectory,
        [Parameter(Mandatory = $true)][int] $TimeoutMilliseconds
    )

    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    do {
        $remaining = if (Test-Path -LiteralPath $InstallDirectory -PathType Container) {
            @(Get-ChildItem -LiteralPath $InstallDirectory -File -Recurse -ErrorAction SilentlyContinue)
        } else {
            @()
        }
        if (@($remaining).Count -eq 0) {
            return [ordered]@{
                installRootRemoved = -not (Test-Path -LiteralPath $InstallDirectory)
                remainingFiles = @()
            }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    return [ordered]@{
        installRootRemoved = -not (Test-Path -LiteralPath $InstallDirectory)
        remainingFiles = @($remaining | ForEach-Object { $_.Name })
    }
}

$installerFile = Get-Item -LiteralPath $Installer
if (-not $installerFile.PSIsContainer -and $installerFile.Extension -ieq '.exe') {
    $installerFile = [System.IO.FileInfo] $installerFile
} else {
    throw "Installer must be an .exe file: $Installer"
}
$outputParent = Split-Path -Parent $Output
if ([string]::IsNullOrWhiteSpace($outputParent)) { $outputParent = (Get-Location).Path }
New-Item -ItemType Directory -Force -Path $outputParent | Out-Null
$outputFile = [System.IO.FileInfo]::new((Resolve-Path -LiteralPath $outputParent).Path + '\' + (Split-Path -Leaf $Output))
$timeoutMilliseconds = $TimeoutSeconds * 1000
$tempBase = if ($env:RUNNER_TEMP -and (Test-Path -LiteralPath $env:RUNNER_TEMP -PathType Container)) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$rootPath = Join-Path $tempBase ('ja-installer-smoke-' + [guid]::NewGuid().ToString('N'))
$installPath = Join-Path $rootPath 'install'
$root = [System.IO.DirectoryInfo]::new($rootPath)
$result = [ordered]@{
    schemaVersion = 1
    product = 'JA'
    sourceCommit = $SourceCommit.ToLowerInvariant()
    engineeringSmokeOnly = $true
    releaseGate = $false
    platform = 'windows'
    architecture = 'x86_64'
    installer = $null
    installation = [ordered]@{ status = 'pending'; files = @(); app = $null; sidecar = $null }
    launch = [ordered]@{ status = 'pending'; pid = $null; processTreeReaped = $false }
    uninstall = [ordered]@{ status = 'pending'; exitCode = $null; installRootRemoved = $false; installFilesRemoved = $false; remainingFiles = @() }
    cleanup = [ordered]@{ status = 'pending'; temporaryRootRemoved = $false }
    signing = [ordered]@{ installer = 'unknown'; installedBinaries = @(); notarizationStatus = 'not-applicable' }
}
$failure = $null

try {
    New-Item -ItemType Directory -Force -Path $rootPath | Out-Null
    $root.Refresh()
    $result.installer = Get-FileEvidence -File ([System.IO.FileInfo] $installerFile)
    $result.signing.installer = $result.installer.signingStatus
    if ($RequireUnsigned -and $result.installer.signingStatus -ne 'NotSigned') {
        throw "Engineering smoke requires an unsigned installer; actual status: $($result.installer.signingStatus)"
    }

    $installResult = Invoke-BoundedProcess -FilePath $installerFile.FullName -ArgumentList @('/S', ('/D=' + $installPath)) -WorkingDirectory $rootPath -TimeoutMilliseconds $timeoutMilliseconds
    if ($installResult.exitCode -ne 0) { throw "NSIS installation failed: exit=$($installResult.exitCode)" }
    if (-not (Test-Path -LiteralPath $installPath -PathType Container)) { throw 'NSIS did not create the temporary install directory' }
    Assert-ContainedPath -Root $root -Candidate $installPath

    $installedFiles = @(Get-ChildItem -LiteralPath $installPath -File -Recurse)
    if ($installedFiles.Count -eq 0) { throw 'Install directory contains no files' }
    $evidenceFiles = foreach ($file in $installedFiles) {
        Assert-ContainedPath -Root $root -Candidate $file.FullName
        $entry = Get-FileEvidence -File ([System.IO.FileInfo] $file)
        $entry.relativePath = (Get-PortableRelativePath -BasePath $installPath -ChildPath $file.FullName).Replace('\', '/')
        $entry
    }
    $result.installation.files = @($evidenceFiles)
    $app = Get-Item -LiteralPath (Join-Path $installPath 'ja.exe') -ErrorAction Stop
    $uninstaller = Get-Item -LiteralPath (Join-Path $installPath 'uninstall.exe') -ErrorAction Stop
    $sidecarFiles = @(Get-ChildItem -LiteralPath (Join-Path $installPath 'sidecars') -Filter 'ja-agent-*.exe' -File)
    if ($sidecarFiles.Count -ne 1) { throw "Unexpected sidecar count in install directory: $($sidecarFiles.Count)" }
    $result.installation.app = Get-FileEvidence -File ([System.IO.FileInfo] $app)
    $result.installation.sidecar = Get-FileEvidence -File ([System.IO.FileInfo] $sidecarFiles[0])
    $result.signing.installedBinaries = @($result.installation.app.signingStatus, $result.installation.sidecar.signingStatus)
    if ($RequireUnsigned -and ($result.signing.installedBinaries | Where-Object { $_ -ne 'NotSigned' })) {
        throw 'Engineering smoke requires the installed app and sidecar to be unsigned'
    }
    $result.installation.status = 'passed'

    $appProcess = Start-Process -FilePath $app.FullName -WorkingDirectory $installPath -PassThru
    try {
        Start-Sleep -Milliseconds 1500
        $appProcess.Refresh()
        if ($appProcess.HasExited) { throw "Installed app exited early: exit=$($appProcess.ExitCode)" }
        $result.launch.pid = [int] $appProcess.Id
        $result.launch.status = 'passed'
    }
    finally {
        Stop-ProcessTree -ProcessId $appProcess.Id
        $result.launch.processTreeReaped = $true
        $appProcess.Dispose()
    }

    $uninstallResult = Invoke-BoundedProcess -FilePath $uninstaller.FullName -ArgumentList @('/S') -WorkingDirectory $rootPath -TimeoutMilliseconds $timeoutMilliseconds
    $result.uninstall.exitCode = $uninstallResult.exitCode
    if ($uninstallResult.exitCode -ne 0) { throw "NSIS uninstall failed: exit=$($uninstallResult.exitCode)" }
    $removal = Wait-ForInstallFilesRemoved -InstallDirectory $installPath -TimeoutMilliseconds ([Math]::Min($timeoutMilliseconds, 10000))
    $remainingFiles = @($removal.remainingFiles)
    $result.uninstall.installRootRemoved = $removal.installRootRemoved
    $result.uninstall.installFilesRemoved = ($remainingFiles.Count -eq 0)
    $result.uninstall.remainingFiles = $remainingFiles
    if (-not $result.uninstall.installFilesRemoved) { throw "Files remain after NSIS uninstall: $($remainingFiles -join ', ')" }
    $result.uninstall.status = 'passed'
}
catch {
    $failure = $_
    $result.error = $_.Exception.Message
}
finally {
    if (Test-Path -LiteralPath $rootPath) {
        Remove-Item -LiteralPath $rootPath -Recurse -Force -ErrorAction SilentlyContinue
    }
    $result.cleanup.temporaryRootRemoved = -not (Test-Path -LiteralPath $rootPath)
    $result.cleanup.status = if ($result.cleanup.temporaryRootRemoved) { 'passed' } else { 'failed' }
    Write-Evidence -Document $result -Destination $outputFile
}

if ($failure -or $result.cleanup.status -ne 'passed') {
    $message = if ($failure) { $failure.Exception.Message } else { 'Temporary root cleanup failed' }
    throw $message
}

if ($result.launch.status -ne 'passed' -or $result.uninstall.status -ne 'passed') {
    throw 'Windows installer smoke did not complete launch and uninstall acceptance'
}

$result | ConvertTo-Json -Depth 8
