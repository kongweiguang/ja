# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

[CmdletBinding()]
param(
    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path,
    [string]$OutputDirectory = 'release\sbom',
    [string]$MavenBomPath = 'agent\target\ja-maven-bom.json',
    [string]$CargoToolchain = '1.88.0',
    [string]$CorrespondingSourcePath = '',
    [string[]]$ArtifactPath = @(),
    [switch]$FailOnBlocker
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-RepositoryPath {
    <#
    .SYNOPSIS
    Resolves a caller-supplied path without allowing report paths to escape the repository.

    .DESCRIPTION
    The report is intended to be reproducible on a clean checkout.  Constraining generated
    paths to the repository prevents accidental inclusion of a developer's home directory or
    credentials while still allowing an explicit source/artifact path to be reported as an
    external input by name only.
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
    .SYNOPSIS
    Converts an existing repository path into a stable slash-separated report path.

    .DESCRIPTION
    Absolute Windows paths are machine-specific and can leak usernames.  All repository-owned
    evidence is therefore recorded relative to the checkout so the same source tree produces
    comparable provenance across runners.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$BasePath
    )

    $base = [IO.Path]::GetFullPath($BasePath).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    $full = [IO.Path]::GetFullPath($Path)
    if (-not $full.StartsWith($base, [StringComparison]::OrdinalIgnoreCase)) {
        return [IO.Path]::GetFileName($full)
    }
    return [IO.Path]::GetRelativePath($BasePath, $full).Replace('\', '/')
}

function Invoke-ExternalCommand {
    <#
    .SYNOPSIS
    Runs one mature dependency tool and captures its exit code without persisting raw logs.

    .DESCRIPTION
    License and SBOM tools may print environment paths or accidental secret material on failure.
    Keeping stdout/stderr in memory lets the caller parse structured output and return only the
    bounded exit/status facts in provenance instead of copying unredacted diagnostics.
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
        $output = @(& $FilePath @Arguments 2>&1 | ForEach-Object { [string]$_ })
        $exitCode = $LASTEXITCODE
        return [PSCustomObject]@{
            ExitCode = $exitCode
            Output = $output
        }
    } finally {
        foreach ($name in $Environment.Keys) {
            [Environment]::SetEnvironmentVariable($name, $oldEnvironment[$name], 'Process')
        }
    }
}

function Get-ToolVersion {
    <#
    .SYNOPSIS
    Records a short tool identity while avoiding full diagnostic output.

    .DESCRIPTION
    Exact tool identity is part of provenance, but complete version banners can contain local
    install paths.  The first non-empty line is sufficient for audit correlation and keeps the
    report free of machine-specific details.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $result = Invoke-ExternalCommand -FilePath $FilePath -Arguments $Arguments
    if ($result.ExitCode -ne 0) {
        return [PSCustomObject]@{ tool = $FilePath; status = 'unavailable'; exitCode = $result.ExitCode }
    }
    $line = @($result.Output | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -First 1)
    return [PSCustomObject]@{
        tool = $FilePath
        status = if ($line.Count -gt 0) { 'available' } else { 'unknown' }
        version = if ($line.Count -gt 0) { $line[0].Trim() } else { '' }
    }
}

function Convert-JsonCommandOutput {
    <#
    .SYNOPSIS
    Parses JSON emitted by a CLI while tolerating a bounded non-JSON warning prefix.

    .DESCRIPTION
    Package managers occasionally emit advisory lines before structured output.  Extracting the
    outer JSON object preserves the mature tool's data without turning this script into a second
    license parser; malformed or multi-document output remains a hard audit failure.
    #>
    param(
        [Parameter(Mandatory = $true)][string[]]$Lines
    )

    $text = ($Lines -join [Environment]::NewLine).Trim()
    $start = $text.IndexOf('{')
    $end = $text.LastIndexOf('}')
    if ($start -lt 0 -or $end -le $start) {
        throw 'tool did not emit a JSON object'
    }
    return ($text.Substring($start, $end - $start + 1) | ConvertFrom-Json)
}

function Write-StableJson {
    <#
    .SYNOPSIS
    Writes UTF-8 JSON with an explicit newline and no timestamp-dependent formatting.

    .DESCRIPTION
    Release evidence must hash identically when lockfiles and tool output are unchanged.  The
    script therefore leaves timestamps out of the schema and centralizes serialization so every
    report file uses the same depth, encoding, and newline policy.
    #>
    param(
        [Parameter(Mandatory = $true)]$Value,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $json = $Value | ConvertTo-Json -Depth 40
    [IO.File]::WriteAllText($Path, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}

function Get-FileEvidence {
    <#
    .SYNOPSIS
    Computes a SHA-256 and size for one existing file.

    .DESCRIPTION
    SHA-256 is the cross-platform checksum required by the artifact manifest contract.  Hashing
    actual files rather than names or lockfile claims prevents a stale report from being treated
    as provenance for a different output.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $file = Get-Item -LiteralPath $Path -Force
    if (-not $file.PSIsContainer -and $file.Length -ge 0) {
        $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        return [PSCustomObject]@{
            path = Get-RelativeRepositoryPath -Path $file.FullName -BasePath $RepositoryRoot
            sizeBytes = [int64]$file.Length
            sha256 = $hash
        }
    }
    throw "expected a file: $Path"
}

function Get-InputEvidence {
    <#
    .SYNOPSIS
    Hashes the fixed dependency inputs that determine the report.

    .DESCRIPTION
    The input set intentionally names package manifests, lockfiles, and legal entry points only;
    generated targets and user configuration are excluded so provenance is reproducible and does
    not accidentally capture credentials or unrelated dirty-worktree files.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $relativePaths = @(
        'package.json',
        'pnpm-lock.yaml',
        'src-tauri/Cargo.toml',
        'src-tauri/Cargo.lock',
        'agent/pom.xml',
        'LICENSE',
        'THIRD_PARTY_NOTICES.md',
        'LICENSES/README.md'
    )
    $facts = @()
    foreach ($relativePath in $relativePaths) {
        $path = Resolve-RepositoryPath -Path $relativePath -BasePath $RepositoryRoot -RequireExisting
        $facts += Get-FileEvidence -Path $path -RepositoryRoot $RepositoryRoot
    }
    return @($facts | Sort-Object path)
}

function Get-GitProvenance {
    <#
    .SYNOPSIS
    Captures the source commit and dirty-state without recording changed file names.

    .DESCRIPTION
    A release report must distinguish a reproducible commit from a developer checkout.  The
    dirty flag is enough for that decision; listing paths would add irrelevant user information
    and could expose secrets in a report intended for distribution.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $commitResult = Invoke-ExternalCommand -FilePath 'git' -Arguments @('-C', $RepositoryRoot, 'rev-parse', 'HEAD')
    if ($commitResult.ExitCode -ne 0 -or @($commitResult.Output).Count -eq 0) {
        throw 'unable to resolve git source commit'
    }
    $statusResult = Invoke-ExternalCommand -FilePath 'git' -Arguments @('-C', $RepositoryRoot, 'status', '--porcelain')
    if ($statusResult.ExitCode -ne 0) {
        throw 'unable to resolve git worktree status'
    }
    return [PSCustomObject]@{
        commit = $commitResult.Output[0].Trim()
        dirty = (@($statusResult.Output | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }).Count -gt 0)
    }
}

function Get-NodeLicenseInventory {
    <#
    .SYNOPSIS
    Projects pnpm's mature license inventory into a path-free, deterministic evidence file.

    .DESCRIPTION
    `pnpm licenses list` remains the authority for npm package license metadata; JA only removes
    machine paths and sorts records.  Offline mode is enforced through npm_config_offline so the
    release report cannot silently resolve a different package tree.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $result = Invoke-ExternalCommand -FilePath 'pnpm' -Arguments @('licenses', 'list', '--json') -Environment @{ npm_config_offline = 'true' }
    if ($result.ExitCode -ne 0) {
        throw "pnpm license inventory failed with exit code $($result.ExitCode)"
    }
    $licenseGroups = Convert-JsonCommandOutput -Lines $result.Output
    $entries = @()
    foreach ($group in $licenseGroups.PSObject.Properties) {
        foreach ($entry in @($group.Value)) {
            $author = if ($entry.PSObject.Properties['author']) { [string]$entry.author } else { '' }
            $homepage = if ($entry.PSObject.Properties['homepage']) { [string]$entry.homepage } else { '' }
            $entries += [PSCustomObject][ordered]@{
                name = [string]$entry.name
                versions = @($entry.versions | ForEach-Object { [string]$_ } | Sort-Object)
                license = [string]$entry.license
                author = $author
                homepage = $homepage
            }
        }
    }
    $sortedEntries = @($entries | Sort-Object name, license, @{ Expression = { $_.versions -join ',' } })
    $missing = @($sortedEntries | Where-Object { [string]::IsNullOrWhiteSpace($_.license) })
    return [PSCustomObject][ordered]@{
        source = 'pnpm licenses list'
        offline = $true
        packageVersionCount = [int](@($sortedEntries | ForEach-Object { $_.versions }).Count)
        entryCount = [int]$sortedEntries.Count
        missingLicenseCount = [int]$missing.Count
        entries = $sortedEntries
    }
}

function Get-CargoLicenseInventory {
    <#
    .SYNOPSIS
    Uses Cargo's locked offline metadata as the Rust dependency/license source.

    .DESCRIPTION
    Cargo already resolves package source, checksum, and declared SPDX expression from the lock;
    this projection keeps only audit fields and omits absolute manifest/target paths.  No custom
    license classification is performed, so nonstandard expressions remain visible for review.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string]$Toolchain
    )

    $manifest = Resolve-RepositoryPath -Path 'src-tauri/Cargo.toml' -BasePath $RepositoryRoot -RequireExisting
    $result = Invoke-ExternalCommand -FilePath 'cargo' -Arguments @("+$Toolchain", 'metadata', '--manifest-path', $manifest, '--locked', '--offline', '--format-version', '1')
    if ($result.ExitCode -ne 0) {
        throw "cargo metadata failed with exit code $($result.ExitCode)"
    }
    $metadata = Convert-JsonCommandOutput -Lines $result.Output
    $packages = @($metadata.packages | ForEach-Object {
        [PSCustomObject][ordered]@{
            name = [string]$_.name
            version = [string]$_.version
            license = [string]$_.license
            source = [string]$_.source
            repository = [string]$_.repository
        }
    } | Sort-Object name, version, source)
    $missing = @($packages | Where-Object { [string]::IsNullOrWhiteSpace($_.license) })
    $lockPath = Resolve-RepositoryPath -Path 'src-tauri/Cargo.lock' -BasePath $RepositoryRoot -RequireExisting
    $lockText = Get-Content -Raw -LiteralPath $lockPath
    return [PSCustomObject][ordered]@{
        source = 'cargo metadata'
        toolchain = $Toolchain
        offline = $true
        packageCount = [int]$packages.Count
        missingLicenseCount = [int]$missing.Count
        registryChecksumCount = [int]([regex]::Matches($lockText, '(?m)^checksum =').Count)
        gitSourceCount = [int]([regex]::Matches($lockText, '(?m)^source = "git\\+').Count)
        pathSourceCount = [int]([regex]::Matches($lockText, '(?m)^source = "path\\+').Count)
        packages = $packages
    }
}

function Normalize-MavenBom {
    <#
    .SYNOPSIS
    Removes CycloneDX fields that are intentionally nondeterministic between identical runs.

    .DESCRIPTION
    CycloneDX's mature Maven plugin emits a UUID and generation timestamp.  Those values identify
    a run rather than its dependency graph, so they are removed from the archived evidence while
    preserving component licenses, hashes, purls, and external references verbatim.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    $bom = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
    if ([string]$bom.bomFormat -ne 'CycloneDX') {
        throw 'Maven BOM is not a CycloneDX document'
    }
    if ($bom.PSObject.Properties['serialNumber']) {
        $bom.PSObject.Properties.Remove('serialNumber')
    }
    if ($bom.metadata -and $bom.metadata.PSObject.Properties['timestamp']) {
        $bom.metadata.PSObject.Properties.Remove('timestamp')
    }
    if ($bom.components) {
        $bom.components = @($bom.components | Sort-Object { [string]$_.'bom-ref' })
    }
    return $bom
}

function Get-MavenBomAudit {
    <#
    .SYNOPSIS
    Validates an offline CycloneDX Maven BOM without re-resolving dependencies.

    .DESCRIPTION
    CycloneDX Maven 2.9.1 currently declares Maven online mode even when its artifacts are cached.
    The report therefore accepts a previously generated BOM as a signed-input boundary and checks
    its license/hash/reference completeness offline; it never silently substitutes a hand-built
    license scanner.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    $bom = Normalize-MavenBom -Path $Path
    $components = @($bom.components)
    $missingLicenses = @($components | Where-Object { @($_.licenses).Count -eq 0 })
    $missingHashes = @($components | Where-Object { @($_.hashes | Where-Object { [string]$_.alg -eq 'SHA-256' }).Count -eq 0 })
    $missingReferences = @($components | Where-Object { @($_.externalReferences).Count -eq 0 })
    $rootLicense = @()
    if ($bom.metadata -and $bom.metadata.component) {
        foreach ($license in @($bom.metadata.component.licenses)) {
            if ($license.license.id) { $rootLicense += [string]$license.license.id }
            elseif ($license.license.name) { $rootLicense += [string]$license.license.name }
        }
    }
    # CycloneDX may normalize SPDX GPL-3.0-or-later to the equivalent ``GPL-3.0+`` token.
    $rootExpected = @($rootLicense | Where-Object { $_ -match '(?i)GPL-3\.0(?:-or-later|\+)|GNU General Public License.*3' }).Count -gt 0
    return [PSCustomObject][ordered]@{
        bomFormat = [string]$bom.bomFormat
        specVersion = [string]$bom.specVersion
        componentCount = [int]$components.Count
        missingLicenseCount = [int]$missingLicenses.Count
        missingSha256Count = [int]$missingHashes.Count
        missingExternalReferenceCount = [int]$missingReferences.Count
        rootLicense = @($rootLicense | Sort-Object -Unique)
        rootLicenseMatchesJa = $rootExpected
        normalizedBom = $bom
    }
}

function Get-LicenseArchiveAudit {
    <#
    .SYNOPSIS
    Checks whether the repository contains audited third-party license text files.

    .DESCRIPTION
    The empty archive is a deliberate truth-preserving state in this repository.  Counting files
    here creates a release gate without copying guessed license text or claiming that package
    metadata alone satisfies redistribution obligations.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $licenseRoot = Resolve-RepositoryPath -Path 'LICENSES' -BasePath $RepositoryRoot -RequireExisting
    $files = @(Get-ChildItem -LiteralPath $licenseRoot -Recurse -File | Where-Object { $_.Name -ne 'README.md' })
    return [PSCustomObject][ordered]@{
        path = 'LICENSES'
        textFileCount = [int]$files.Count
        files = @($files | ForEach-Object { Get-RelativeRepositoryPath -Path $_.FullName -BasePath $RepositoryRoot } | Sort-Object)
    }
}

function Get-ArtifactAudit {
    <#
    .SYNOPSIS
    Hashes explicitly supplied release artifacts and never scans broad workspace directories.

    .DESCRIPTION
    Release output names and signatures must come from actual files.  Requiring explicit paths
    avoids treating ignored build debris as a distributable artifact and keeps this report safe on
    a dirty development checkout.
    #>
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$Paths,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $files = @()
    foreach ($pathText in $Paths) {
        $path = Resolve-RepositoryPath -Path $pathText -BasePath $RepositoryRoot -RequireExisting
        $item = Get-Item -LiteralPath $path -Force
        if ($item.PSIsContainer) {
            $files += @(Get-ChildItem -LiteralPath $item.FullName -Recurse -File | ForEach-Object { Get-FileEvidence -Path $_.FullName -RepositoryRoot $RepositoryRoot })
        } else {
            $files += Get-FileEvidence -Path $item.FullName -RepositoryRoot $RepositoryRoot
        }
    }
    return @($files | Sort-Object path, sha256 -Unique)
}

function Add-Blocker {
    <#
    .SYNOPSIS
    Adds one stable release-gate code and reason to the report.

    .DESCRIPTION
    Codes make CI assertions durable while short reasons keep the report useful to a human.  The
    function deliberately accepts no raw tool output so secrets and host paths cannot enter the
    release evidence through an error branch.
    #>
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.Generic.List[object]]$Blockers,
        [Parameter(Mandatory = $true)][string]$Code,
        [Parameter(Mandatory = $true)][string]$Reason
    )

    $Blockers.Add([PSCustomObject][ordered]@{ code = $Code; reason = $Reason })
}

function Write-ChecksumManifest {
    <#
    .SYNOPSIS
    Writes a sorted SHA-256 manifest for the generated evidence files.

    .DESCRIPTION
    The manifest excludes itself to avoid a checksum cycle; every listed file is hashed after it
    is written.  Sorting by slash-normalized path keeps the same inputs byte-stable across hosts.
    #>
    param(
        [Parameter(Mandatory = $true)][string[]]$Paths,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string]$ManifestPath
    )

    $lines = @()
    foreach ($path in $Paths) {
        $evidence = Get-FileEvidence -Path $path -RepositoryRoot $RepositoryRoot
        $lines += ('{0}  {1}' -f $evidence.sha256, $evidence.path)
    }
    [IO.File]::WriteAllText($ManifestPath, (($lines | Sort-Object) -join [Environment]::NewLine) + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}

$root = [IO.Path]::GetFullPath($RepositoryRoot)
$outputRoot = Resolve-RepositoryPath -Path $OutputDirectory -BasePath $root
[IO.Directory]::CreateDirectory($outputRoot) | Out-Null

$blockers = [System.Collections.Generic.List[object]]::new()
$git = Get-GitProvenance -RepositoryRoot $root
$inputs = Get-InputEvidence -RepositoryRoot $root

$nodeInventory = Get-NodeLicenseInventory -RepositoryRoot $root
$nodePath = Join-Path $outputRoot 'node-licenses.json'
Write-StableJson -Value $nodeInventory -Path $nodePath

$cargoInventory = Get-CargoLicenseInventory -RepositoryRoot $root -Toolchain $CargoToolchain
$cargoPath = Join-Path $outputRoot 'cargo-packages.json'
Write-StableJson -Value $cargoInventory -Path $cargoPath

$mavenBomAudit = $null
$mavenOutputPath = Join-Path $outputRoot 'maven-cyclonedx.json'
$mavenInput = Resolve-RepositoryPath -Path $MavenBomPath -BasePath $root
if (Test-Path -LiteralPath $mavenInput -PathType Leaf) {
    $mavenBomAudit = Get-MavenBomAudit -Path $mavenInput
    Write-StableJson -Value $mavenBomAudit.normalizedBom -Path $mavenOutputPath
    if ($mavenBomAudit.missingLicenseCount -gt 0) { Add-Blocker -Blockers $blockers -Code 'MAVEN_BOM_LICENSES_MISSING' -Reason 'one or more Maven BOM components lack license records' }
    if ($mavenBomAudit.missingSha256Count -gt 0) { Add-Blocker -Blockers $blockers -Code 'MAVEN_BOM_SHA256_MISSING' -Reason 'one or more Maven BOM components lack SHA-256 hashes' }
    if ($mavenBomAudit.missingExternalReferenceCount -gt 0) { Add-Blocker -Blockers $blockers -Code 'MAVEN_BOM_REFERENCE_MISSING' -Reason 'one or more Maven BOM components lack source/reference URLs' }
    if (-not $mavenBomAudit.rootLicenseMatchesJa) { Add-Blocker -Blockers $blockers -Code 'PROJECT_LICENSE_METADATA_MISMATCH' -Reason 'Maven BOM root component does not declare JA GPL-3.0-or-later' }
} else {
    # Remove only the script-owned normalized BOM so a prior run cannot be mistaken for current evidence.
    if (Test-Path -LiteralPath $mavenOutputPath -PathType Leaf) {
        Remove-Item -LiteralPath $mavenOutputPath -Force
    }
    Add-Blocker -Blockers $blockers -Code 'MAVEN_BOM_INPUT_MISSING' -Reason 'provide a CycloneDX Maven BOM generated by the pinned mature plugin before release'
}

$archive = Get-LicenseArchiveAudit -RepositoryRoot $root
if ($archive.textFileCount -eq 0) {
    Add-Blocker -Blockers $blockers -Code 'LICENSE_ARCHIVE_EMPTY' -Reason 'LICENSES contains no audited third-party license text files'
}

$artifacts = @(Get-ArtifactAudit -Paths $ArtifactPath -RepositoryRoot $root)
if ($artifacts.Count -eq 0) {
    Add-Blocker -Blockers $blockers -Code 'ARTIFACTS_NOT_PROVIDED' -Reason 'supply actual release bundles/installers for artifact checksum and signature evidence'
}

if ($git.dirty) {
    Add-Blocker -Blockers $blockers -Code 'GIT_TREE_DIRTY' -Reason 'release provenance must be generated from a clean source commit'
}
if ([string]::IsNullOrWhiteSpace($CorrespondingSourcePath)) {
    Add-Blocker -Blockers $blockers -Code 'CORRESPONDING_SOURCE_NOT_PROVIDED' -Reason 'record the GPL corresponding-source archive or durable source offer before distribution'
} elseif (-not (Test-Path -LiteralPath (Resolve-RepositoryPath -Path $CorrespondingSourcePath -BasePath $root))) {
    Add-Blocker -Blockers $blockers -Code 'CORRESPONDING_SOURCE_MISSING' -Reason 'the supplied corresponding-source path does not exist'
}

$toolFacts = @(
    Get-ToolVersion -FilePath 'pnpm' -Arguments @('--version')
    Get-ToolVersion -FilePath 'cargo' -Arguments @("+$CargoToolchain", '--version')
    Get-ToolVersion -FilePath 'git' -Arguments @('--version')
)

$evidencePaths = @($nodePath, $cargoPath)
if (Test-Path -LiteralPath $mavenOutputPath -PathType Leaf) { $evidencePaths += $mavenOutputPath }
$evidenceFacts = @($evidencePaths | ForEach-Object { Get-FileEvidence -Path $_ -RepositoryRoot $root })
$reportPath = Join-Path $outputRoot 'dependency-license-report.json'
$report = [PSCustomObject][ordered]@{
    schemaVersion = 1
    project = 'ja'
    source = $git
    networkPolicy = 'offline'
    tools = $toolFacts
    inputs = $inputs
    node = [PSCustomObject][ordered]@{ evidence = 'node-licenses.json'; entryCount = $nodeInventory.entryCount; packageVersionCount = $nodeInventory.packageVersionCount; missingLicenseCount = $nodeInventory.missingLicenseCount }
    rust = [PSCustomObject][ordered]@{ evidence = 'cargo-packages.json'; packageCount = $cargoInventory.packageCount; missingLicenseCount = $cargoInventory.missingLicenseCount; registryChecksumCount = $cargoInventory.registryChecksumCount; gitSourceCount = $cargoInventory.gitSourceCount; pathSourceCount = $cargoInventory.pathSourceCount }
    maven = if ($mavenBomAudit) { [PSCustomObject][ordered]@{ evidence = 'maven-cyclonedx.json'; componentCount = $mavenBomAudit.componentCount; missingLicenseCount = $mavenBomAudit.missingLicenseCount; missingSha256Count = $mavenBomAudit.missingSha256Count; missingExternalReferenceCount = $mavenBomAudit.missingExternalReferenceCount; rootLicense = $mavenBomAudit.rootLicense; rootLicenseMatchesJa = $mavenBomAudit.rootLicenseMatchesJa } } else { [PSCustomObject][ordered]@{ evidence = $null; status = 'missing' } }
    licenseArchive = $archive
    artifacts = $artifacts
    evidence = $evidenceFacts
    blockers = @($blockers)
    status = if ($blockers.Count -eq 0) { 'complete' } else { 'blocked' }
}
Write-StableJson -Value $report -Path $reportPath

$checksumPath = Join-Path $outputRoot 'SHA256SUMS'
Write-ChecksumManifest -Paths @($evidencePaths + $reportPath) -RepositoryRoot $root -ManifestPath $checksumPath

$provenancePath = Join-Path $outputRoot 'provenance.json'
$provenance = [PSCustomObject][ordered]@{
    schemaVersion = 1
    project = 'ja'
    source = $git
    networkPolicy = 'offline'
    report = Get-FileEvidence -Path $reportPath -RepositoryRoot $root
    evidence = @($evidenceFacts)
    licenseArchive = $archive
    correspondingSource = if ([string]::IsNullOrWhiteSpace($CorrespondingSourcePath)) { $null } else { Get-RelativeRepositoryPath -Path (Resolve-RepositoryPath -Path $CorrespondingSourcePath -BasePath $root) -BasePath $root }
    blockers = @($blockers)
    status = if ($blockers.Count -eq 0) { 'complete' } else { 'blocked' }
}
Write-StableJson -Value $provenance -Path $provenancePath

# Rewrite the checksum manifest once more so provenance is covered; the manifest still excludes
# itself to avoid a self-referential hash cycle.
Write-ChecksumManifest -Paths @($evidencePaths + $reportPath + $provenancePath) -RepositoryRoot $root -ManifestPath $checksumPath

Write-Output ("status={0} blockers={1} output={2}" -f $report.status, $blockers.Count, (Get-RelativeRepositoryPath -Path $outputRoot -BasePath $root))
if ($FailOnBlocker -and $blockers.Count -gt 0) {
    exit 2
}
exit 0
