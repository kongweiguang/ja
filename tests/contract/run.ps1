# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$env:PYTHONDONTWRITEBYTECODE = "1"
Push-Location $root
try {
    # Delegate all ordering, bounded output, and cleanup invariants to one cross-platform runner.
    & python -B (Join-Path $root "tests/contract/run.py")
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}
finally {
    Pop-Location
}
