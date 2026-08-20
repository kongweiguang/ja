# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

<#
.SYNOPSIS
  Prepare an ephemeral Windows certificate store and Tauri signing overlay.

.DESCRIPTION
  The release workflow keeps certificate material in GitHub Secrets and passes it through
  environment variables only. This script imports the short-lived PFX into the runner's
  current-user store, creates a minimal Tauri config overlay, and writes non-secret paths to
  GITHUB_ENV. It deliberately fails closed when a secret, private key, timestamp URL, or
  signtool is missing; it never prints the certificate, password, or thumbprint.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string] $OutputDirectory,

    [string] $EnvironmentFile = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

<#
.SYNOPSIS
  Require a non-empty secret without exposing its value in an exception or log.

.DESCRIPTION
  Release jobs must stop before a signing command can accidentally fall back to an unsigned
  build. Keeping this check in a small function also makes the secret boundary obvious in code
  review and keeps the workflow itself free of certificate parsing logic.
#>
function Get-RequiredSecret {
    param(
        [Parameter(Mandatory = $true)][string] $Name,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string] $Value
    )

    if ([string]::IsNullOrWhiteSpace($Value)) {
        throw "Required signing secret is missing: $Name"
    }
    return $Value
}

<#
.SYNOPSIS
  Locate the x64 Windows SDK signtool used by Tauri's bundler.

.DESCRIPTION
  Hosted Windows images can carry multiple SDK revisions. The explicit x64 selection avoids
  silently choosing an ARM or obsolete helper while still using the runner-provided tool rather
  than downloading an unpinned executable during release.
#>
function Find-SignTool {
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }

    $roots = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'),
        (Join-Path $env:ProgramFiles 'Windows Kits\10\bin')
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and (Test-Path -LiteralPath $_ -PathType Container) }

    $candidates = @(
        foreach ($root in $roots) {
            Get-ChildItem -LiteralPath $root -Filter 'signtool.exe' -File -Recurse -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' }
        }
    ) | Sort-Object FullName -Descending
    if ($candidates.Count -eq 0) {
        throw 'Windows SDK x64 signtool.exe was not found; refusing to create a signed release.'
    }
    return $candidates[0].FullName
}

<#
.SYNOPSIS
  Append a non-secret environment assignment using GitHub's runner file protocol.

.DESCRIPTION
  Only paths, the certificate thumbprint, and the fixed timestamp URL are emitted. The PFX and
  password remain outside the environment file so later steps cannot accidentally archive them.
#>
function Set-RunnerEnvironment {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $Name,
        [Parameter(Mandatory = $true)][string] $Value
    )

    Add-Content -LiteralPath $Path -Value "$Name=$Value" -Encoding utf8NoBOM
}

$certificateBase64 = Get-RequiredSecret -Name 'WINDOWS_CERTIFICATE' -Value ([string] $env:WINDOWS_CERTIFICATE)
$certificatePassword = Get-RequiredSecret -Name 'WINDOWS_CERTIFICATE_PASSWORD' -Value ([string] $env:WINDOWS_CERTIFICATE_PASSWORD)
$expectedThumbprint = (Get-RequiredSecret -Name 'WINDOWS_CERTIFICATE_THUMBPRINT' -Value ([string] $env:WINDOWS_CERTIFICATE_THUMBPRINT)) -replace '\s', ''
if ($expectedThumbprint -notmatch '^[0-9A-Fa-f]{40}$') {
    throw 'WINDOWS_CERTIFICATE_THUMBPRINT must be a 40-character hexadecimal thumbprint.'
}
$timestampUrl = if ([string]::IsNullOrWhiteSpace([string] $env:WINDOWS_TIMESTAMP_URL)) {
    'http://timestamp.digicert.com'
} else {
    [string] $env:WINDOWS_TIMESTAMP_URL
}
$parsedTimestamp = $null
if (-not [Uri]::TryCreate($timestampUrl, [UriKind]::Absolute, [ref] $parsedTimestamp) -or
    -not $timestampUrl.StartsWith('http', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'WINDOWS_TIMESTAMP_URL must be an absolute HTTP(S) URL.'
}

$output = [IO.DirectoryInfo]::new([IO.Path]::GetFullPath($OutputDirectory))
$output.Create()
$pfxPath = Join-Path $output.FullName 'ja-release-signing.pfx'
$configPath = Join-Path $output.FullName 'tauri-signing.windows.json'
$environmentPath = if ([string]::IsNullOrWhiteSpace($EnvironmentFile)) {
    [string] $env:GITHUB_ENV
} else {
    [IO.Path]::GetFullPath($EnvironmentFile)
}
if ([string]::IsNullOrWhiteSpace($environmentPath)) {
    throw 'A GitHub environment file is required so later signing steps receive only safe paths.'
}

$certificates = @()
try {
    try {
        $bytes = [Convert]::FromBase64String($certificateBase64)
    } catch {
        throw 'WINDOWS_CERTIFICATE is not valid base64.'
    }
    if ($bytes.Length -lt 512) {
        throw 'WINDOWS_CERTIFICATE is unexpectedly small; refusing to import it.'
    }
    [IO.File]::WriteAllBytes($pfxPath, $bytes)

    $securePassword = ConvertTo-SecureString -String $certificatePassword -AsPlainText -Force
    $certificates = @(Import-PfxCertificate -FilePath $pfxPath -CertStoreLocation 'Cert:\CurrentUser\My' -Password $securePassword)
    $certificate = $certificates | Where-Object { $_.HasPrivateKey } | Select-Object -First 1
    if ($null -eq $certificate) {
        throw 'The imported certificate has no private key.'
    }
    $now = [DateTime]::UtcNow
    if ($certificate.NotBefore.ToUniversalTime() -gt $now -or $certificate.NotAfter.ToUniversalTime() -le $now) {
        throw 'The imported signing certificate is not currently valid.'
    }
    $thumbprint = $certificate.Thumbprint -replace '\s', ''
    if ($thumbprint -notmatch '^[0-9A-Fa-f]{40}$') {
        throw 'The imported certificate thumbprint is invalid.'
    }
    if (-not $thumbprint.Equals($expectedThumbprint, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The imported certificate thumbprint does not match the release allowlist.'
    }

    $signTool = Find-SignTool
    $overlay = [ordered]@{
        bundle = [ordered]@{
            windows = [ordered]@{
                certificateThumbprint = $thumbprint.ToUpperInvariant()
                digestAlgorithm = 'sha256'
                timestampUrl = $timestampUrl
            }
        }
    }
    $json = $overlay | ConvertTo-Json -Depth 6
    [IO.File]::WriteAllText($configPath, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))

    Set-RunnerEnvironment -Path $environmentPath -Name 'JA_TAURI_SIGNING_CONFIG' -Value $configPath
    Set-RunnerEnvironment -Path $environmentPath -Name 'TAURI_WINDOWS_SIGNTOOL_PATH' -Value $signTool
    Set-RunnerEnvironment -Path $environmentPath -Name 'JA_WINDOWS_CERT_THUMBPRINT' -Value $thumbprint.ToUpperInvariant()
    Set-RunnerEnvironment -Path $environmentPath -Name 'JA_WINDOWS_CERT_PFX' -Value $pfxPath
    Write-Output 'Windows signing preparation passed; secret values were not emitted.'
} catch {
    foreach ($imported in @($certificates)) {
        $importedThumbprint = ([string] $imported.Thumbprint) -replace '\s', ''
        if ($importedThumbprint -match '^[0-9A-Fa-f]{40}$') {
            Remove-Item -LiteralPath ("Cert:\CurrentUser\My\" + $importedThumbprint) -Force -ErrorAction SilentlyContinue
        }
    }
    if (Test-Path -LiteralPath $pfxPath) {
        Remove-Item -LiteralPath $pfxPath -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $configPath) {
        Remove-Item -LiteralPath $configPath -Force -ErrorAction SilentlyContinue
    }
    throw
}
