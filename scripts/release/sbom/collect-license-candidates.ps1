# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

[CmdletBinding()]
param(
    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path,
    [string]$OutputDirectory = 'release\sbom\license-candidate',
    [string]$MavenBomPath = 'agent\target\ja-maven-bom.json',
    [string]$CargoToolchain = '1.88.0'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-RepositoryPath {
    <#
    Resolves a repository-relative path while keeping generated evidence inside the checkout.
    This prevents a local package cache or user-selected output path from being copied by
    accident into the candidate archive.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$BasePath,
        [switch]$RequireExisting
    )

    $candidate = if ([IO.Path]::IsPathRooted($Path)) {
        [IO.Path]::GetFullPath($Path)
    } else {
        [IO.Path]::GetFullPath((Join-Path $BasePath $Path))
    }
    if ($RequireExisting -and -not (Test-Path -LiteralPath $candidate)) {
        throw "path does not exist: $Path"
    }
    return $candidate
}

function Get-RelativeRepositoryPath {
    <#
    Converts repository-owned paths to stable slash-separated names; absolute cache paths never
    enter the manifest because package records use ecosystem coordinates instead.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$BasePath
    )

    return ([IO.Path]::GetRelativePath($BasePath, $Path)).Replace('\', '/')
}

function Invoke-JsonCommand {
    <#
    Runs an offline dependency command and extracts its one JSON document.  Tool warnings are
    tolerated only before the outer object; malformed or empty output remains a hard failure.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [hashtable]$Environment = @{}
    )

    $oldEnvironment = @{}
    foreach ($name in $Environment.Keys) {
        $oldEnvironment[$name] = [Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name, [string]$Environment[$name], 'Process')
    }
    try {
        $lines = @(& $FilePath @Arguments 2>&1 | ForEach-Object { [string]$_ })
        if ($LASTEXITCODE -ne 0) {
            throw "$FilePath failed with exit code $LASTEXITCODE"
        }
    } finally {
        foreach ($name in $Environment.Keys) {
            [Environment]::SetEnvironmentVariable($name, $oldEnvironment[$name], 'Process')
        }
    }

    $text = ($lines -join [Environment]::NewLine).Trim()
    $start = $text.IndexOf('{')
    $end = $text.LastIndexOf('}')
    if ($start -lt 0 -or $end -le $start) {
        throw "$FilePath did not emit a JSON object"
    }
    return ($text.Substring($start, $end - $start + 1) | ConvertFrom-Json)
}

function Get-OptionalPropertyString {
    <#
    Reads optional metadata without letting StrictMode turn a missing homepage/repository field
    into a failed archive run; absent fields remain explicit empty strings in the manifest.
    #>
    param(
        [Parameter(Mandatory = $true)]$Object,
        [Parameter(Mandatory = $true)][string]$Name
    )

    if ($Object.PSObject.Properties[$Name]) {
        return [string]$Object.$Name
    }
    return ''
}

function Get-LicenseFiles {
    <#
    Finds license/notice files supplied by one dependency without descending into nested package
    installations.  The bytes are copied unchanged; no SPDX expression is inferred here.
    #>
    param([Parameter(Mandatory = $true)][string]$PackageRoot)

    $pattern = '^(LICENSE|LICENCE|COPYING|NOTICE)([._-].*)?$'
    return @(Get-ChildItem -LiteralPath $PackageRoot -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object {
            $relative = [IO.Path]::GetRelativePath($PackageRoot, $_.FullName)
            $_.Name -match $pattern -and
            $relative -notmatch '(?i)(^|[\\/])node_modules([\\/]|$)'
        } | Sort-Object FullName)
}

function New-LicenseCollector {
    <#
    Creates the collector state.  Hash-addressed text deduplicates identical MIT/Apache/etc.
    files while the manifest retains every package-to-text mapping needed for legal review.
    #>
    param([Parameter(Mandatory = $true)][string]$OutputRoot)

    $textRoot = Join-Path $OutputRoot 'text'
    [IO.Directory]::CreateDirectory($textRoot) | Out-Null
    return [PSCustomObject]@{
        OutputRoot = $OutputRoot
        TextRoot = $textRoot
        Blobs = @{}
        Records = [System.Collections.Generic.List[object]]::new()
        Missing = [System.Collections.Generic.List[object]]::new()
    }
}

function Add-LicenseBytes {
    <#
    Persists one exact byte sequence under its SHA-256 and appends a package mapping.  Existing
    hashes are reused, so the candidate remains compact without altering upstream license text.
    #>
    param(
        [Parameter(Mandatory = $true)]$Collector,
        [Parameter(Mandatory = $true)][byte[]]$Bytes,
        [Parameter(Mandatory = $true)][string]$Ecosystem,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$DeclaredLicense,
        [Parameter(Mandatory = $true)][string]$SourceFile,
        [string]$Repository = '',
        [string]$Homepage = ''
    )

    $sha = ([Security.Cryptography.SHA256]::Create().ComputeHash($Bytes) |
        ForEach-Object { $_.ToString('x2') }) -join ''
    if (-not $Collector.Blobs.ContainsKey($sha)) {
        $target = Join-Path $Collector.TextRoot "$sha.txt"
        [IO.File]::WriteAllBytes($target, $Bytes)
        $Collector.Blobs[$sha] = $target
    }
    $Collector.Records.Add([PSCustomObject][ordered]@{
        ecosystem = $Ecosystem
        name = $Name
        version = $Version
        declaredLicense = $DeclaredLicense
        repository = $Repository
        homepage = $Homepage
        sourceFile = $SourceFile
        archiveFile = "text/$sha.txt"
        sha256 = $sha
        sizeBytes = [int64]$Bytes.Length
    })
}

function Add-LicenseFile {
    <#
    Reads a local license/notice file as bytes and delegates to the hash-addressed collector.
    Reading bytes instead of normalized text preserves upstream line endings and notices.
    #>
    param(
        [Parameter(Mandatory = $true)]$Collector,
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Ecosystem,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$DeclaredLicense,
        [string]$Repository = '',
        [string]$Homepage = ''
    )

    Add-LicenseBytes -Collector $Collector -Bytes ([IO.File]::ReadAllBytes($Path) `
        ) -Ecosystem $Ecosystem -Name $Name -Version $Version `
        -DeclaredLicense $DeclaredLicense -SourceFile ([IO.Path]::GetFileName($Path)) `
        -Repository $Repository -Homepage $Homepage
}

function Add-MissingRecord {
    <#
    Records a package whose local cache has no license/notice bytes.  Missing records keep the
    candidate honest and provide the exact follow-up list for a controlled source/legal review.
    #>
    param(
        [Parameter(Mandatory = $true)]$Collector,
        [Parameter(Mandatory = $true)][string]$Ecosystem,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$DeclaredLicense,
        [string]$Repository = '',
        [string]$Homepage = '',
        [string]$Reason = 'no local license or notice file found'
    )

    $Collector.Missing.Add([PSCustomObject][ordered]@{
        ecosystem = $Ecosystem
        name = $Name
        version = $Version
        declaredLicense = $DeclaredLicense
        repository = $Repository
        homepage = $Homepage
        reason = $Reason
    })
}

function Get-DeclaredLicenses {
    <#
    Converts a CycloneDX license array to one stable expression for the candidate manifest; the
    original BOM remains the authority for the detailed component record.
    #>
    param([Parameter(Mandatory = $true)]$Licenses)

    return (@($Licenses | ForEach-Object {
        if (-not $_.PSObject.Properties['license']) { return }
        if ($_.license.PSObject.Properties['id'] -and $_.license.id) { [string]$_.license.id }
        elseif ($_.license.PSObject.Properties['name'] -and $_.license.name) { [string]$_.license.name }
    } | Sort-Object -Unique) -join ' OR ')
}

function Add-MavenArchiveEntries {
    <#
    Reads license/notice entries directly from the cached Maven JAR.  A missing JAR entry is
    reported rather than replaced with a guessed SPDX text, because POM metadata alone is not
    enough to prove the exact notice obligations of a component.
    #>
    param(
        [Parameter(Mandatory = $true)]$Collector,
        [Parameter(Mandatory = $true)]$Component,
        [Parameter(Mandatory = $true)][string]$MavenRepository
    )

    $purl = [string]$Component.purl
    if ($purl -notmatch '^pkg:maven/(?<group>[^/]+)/(?<artifact>[^@]+)@(?<version>[^?]+)') {
        Add-MissingRecord -Collector $Collector -Ecosystem 'maven' -Name ([string]$Component.name) `
            -Version ([string]$Component.version) -DeclaredLicense (Get-DeclaredLicenses $Component.licenses) `
            -Reason 'CycloneDX component has no parseable Maven purl'
        return
    }
    $group = [Uri]::UnescapeDataString($Matches.group)
    $artifact = [Uri]::UnescapeDataString($Matches.artifact)
    $version = [Uri]::UnescapeDataString($Matches.version)
    $jarPath = Join-Path $MavenRepository (($group -replace '\.', '\') + "\$artifact\$version\$artifact-$version.jar")
    if (-not (Test-Path -LiteralPath $jarPath -PathType Leaf)) {
        Add-MissingRecord -Collector $Collector -Ecosystem 'maven' -Name $artifact -Version $version `
            -DeclaredLicense (Get-DeclaredLicenses $Component.licenses) -Reason 'cached Maven JAR not found'
        return
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($jarPath)
    try {
        $entries = @($zip.Entries | Where-Object {
            -not [string]::IsNullOrWhiteSpace($_.Name) -and
            $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)([._-].*)?$'
        })
        if ($entries.Count -eq 0) {
            Add-MissingRecord -Collector $Collector -Ecosystem 'maven' -Name $artifact -Version $version `
                -DeclaredLicense (Get-DeclaredLicenses $Component.licenses) -Reason 'Maven JAR has no license/notice entry'
            return
        }
        foreach ($entry in $entries) {
            $stream = $entry.Open()
            $memory = [IO.MemoryStream]::new()
            try { $stream.CopyTo($memory) } finally { $stream.Dispose() }
            Add-LicenseBytes -Collector $Collector -Bytes $memory.ToArray() -Ecosystem 'maven' `
                -Name $artifact -Version $version -DeclaredLicense (Get-DeclaredLicenses $Component.licenses) `
                -SourceFile $entry.FullName -Repository ([string]$Component.purl)
            $memory.Dispose()
        }
    } finally {
        $zip.Dispose()
    }
}

function Write-CandidateManifest {
    <#
    Writes a deterministic candidate manifest and review README.  The explicit pending status is
    intentional: this output is evidence for a release owner, not an automatic legal approval.
    #>
    param(
        [Parameter(Mandatory = $true)]$Collector,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string]$MavenBomPath,
        [Parameter(Mandatory = $true)][string]$CargoToolchain
    )

    $manifest = [PSCustomObject][ordered]@{
        schemaVersion = 1
        status = 'candidate-review-pending'
        networkPolicy = 'offline'
        generatedBy = 'scripts/release/sbom/collect-license-candidates.ps1'
        inputs = [PSCustomObject][ordered]@{
            packageManifest = 'package.json'
            npmLock = 'pnpm-lock.yaml'
            cargoManifest = 'src-tauri/Cargo.toml'
            cargoLock = 'src-tauri/Cargo.lock'
            mavenBom = Get-RelativeRepositoryPath -Path (Resolve-RepositoryPath -Path $MavenBomPath -BasePath $RepositoryRoot) -BasePath $RepositoryRoot
            cargoToolchain = $CargoToolchain
        }
        summary = [PSCustomObject][ordered]@{
            uniqueTextFiles = [int]$Collector.Blobs.Count
            mappingCount = [int]$Collector.Records.Count
            missingTextCount = [int]$Collector.Missing.Count
        }
        mappings = @($Collector.Records | Sort-Object ecosystem, name, version, sourceFile, sha256)
        missing = @($Collector.Missing | Sort-Object ecosystem, name, version)
    }
    $jsonPath = Join-Path $Collector.OutputRoot 'manifest.json'
    $json = $manifest | ConvertTo-Json -Depth 20
    [IO.File]::WriteAllText($jsonPath, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
    $readme = @'
<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA 第三方许可证候选归档

本目录由 `collect-license-candidates.ps1` 从当前 lockfile、CycloneDX Maven BOM 和本机
离线缓存复制原始 license/notice 字节生成。`text/<sha256>.txt` 是去重后的原文，
`manifest.json` 保留每个组件到原文的映射和缺失项。

这不是法律批准，也不是发布归档。发布前必须在 clean checkout 固定输入，补齐
`missing`，核对版权、NOTICE、GPL 兼容性、Native/Tauri 实际再分发边界，并由发布 owner
把已审计文件迁入 `LICENSES/approved/` 后再关闭供应链门。
'@
    [IO.File]::WriteAllText((Join-Path $Collector.OutputRoot 'README.md'), $readme + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
    return $manifest
}

$root = [IO.Path]::GetFullPath($RepositoryRoot)
$outputRoot = Resolve-RepositoryPath -Path $OutputDirectory -BasePath $root
if (Test-Path -LiteralPath $outputRoot) {
    throw "candidate output already exists; choose a new OutputDirectory: $OutputDirectory"
}
[IO.Directory]::CreateDirectory($outputRoot) | Out-Null
$collector = New-LicenseCollector -OutputRoot $outputRoot

$npm = Invoke-JsonCommand -FilePath 'pnpm' -Arguments @('licenses', 'list', '--json') `
    -Environment @{ npm_config_offline = 'true' }
foreach ($group in $npm.PSObject.Properties) {
    foreach ($entry in @($group.Value)) {
        $packageRootFound = $false
        foreach ($packagePath in @($entry.paths)) {
            $files = @(Get-LicenseFiles -PackageRoot ([string]$packagePath))
            foreach ($file in $files) {
                $packageRootFound = $true
                Add-LicenseFile -Collector $collector -Path $file.FullName -Ecosystem 'npm' `
                    -Name ([string]$entry.name) -Version ((@($entry.versions) | ForEach-Object { [string]$_ }) -join ',') `
                    -DeclaredLicense ([string]$entry.license) -Homepage (Get-OptionalPropertyString -Object $entry -Name 'homepage')
            }
        }
        if (-not $packageRootFound) {
            Add-MissingRecord -Collector $collector -Ecosystem 'npm' -Name ([string]$entry.name) `
                -Version ((@($entry.versions) | ForEach-Object { [string]$_ }) -join ',') `
                -DeclaredLicense ([string]$entry.license) -Homepage (Get-OptionalPropertyString -Object $entry -Name 'homepage')
        }
    }
}

$cargoManifest = Resolve-RepositoryPath -Path 'src-tauri/Cargo.toml' -BasePath $root -RequireExisting
$cargo = Invoke-JsonCommand -FilePath 'cargo' -Arguments @("+$CargoToolchain", 'metadata', '--manifest-path', $cargoManifest, '--locked', '--offline', '--format-version', '1')
foreach ($package in @($cargo.packages)) {
    $manifestPath = [string]$package.manifest_path
    $packageRoot = Split-Path -Parent $manifestPath
    $files = @(Get-LicenseFiles -PackageRoot $packageRoot)
    if ($files.Count -eq 0) {
        Add-MissingRecord -Collector $collector -Ecosystem 'cargo' -Name ([string]$package.name) `
            -Version ([string]$package.version) -DeclaredLicense ([string]$package.license) `
            -Repository (Get-OptionalPropertyString -Object $package -Name 'repository')
        continue
    }
    foreach ($file in $files) {
        Add-LicenseFile -Collector $collector -Path $file.FullName -Ecosystem 'cargo' `
            -Name ([string]$package.name) -Version ([string]$package.version) `
            -DeclaredLicense ([string]$package.license) -Repository (Get-OptionalPropertyString -Object $package -Name 'repository')
    }
}

$bom = Resolve-RepositoryPath -Path $MavenBomPath -BasePath $root -RequireExisting
$maven = Get-Content -Raw -LiteralPath $bom | ConvertFrom-Json
$mavenRepository = Join-Path $env:USERPROFILE '.m2\repository'
foreach ($component in @($maven.components)) {
    Add-MavenArchiveEntries -Collector $collector -Component $component -MavenRepository $mavenRepository
}

$manifest = Write-CandidateManifest -Collector $collector -RepositoryRoot $root `
    -MavenBomPath $MavenBomPath -CargoToolchain $CargoToolchain
Write-Output ('status={0} uniqueTextFiles={1} mappings={2} missing={3} output={4}' -f `
    $manifest.status, $manifest.summary.uniqueTextFiles, $manifest.summary.mappingCount, `
    $manifest.summary.missingTextCount, (Get-RelativeRepositoryPath -Path $outputRoot -BasePath $root))
exit 0
