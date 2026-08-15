param(
    [Parameter(Mandatory = $true)]
    [string] $report,
    [Parameter(Mandatory = $true)]
    [Alias('stderr-bytes')]
    [int] $stderrBytes = 0
)

# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$parentSecret = [Environment]::GetEnvironmentVariable('JA_MCP_PARENT_SECRET')
$explicitValue = [Environment]::GetEnvironmentVariable('JA_MCP_EXPLICIT_VALUE')
$resolvedSecret = [Environment]::GetEnvironmentVariable('JA_MCP_RESOLVED_SECRET')
$parentSecretVisible = [bool](-not [string]::IsNullOrEmpty($parentSecret))
$explicitEnvironmentVisible = [bool]($explicitValue -like 'native-explicit-*')
$resolvedSecretVisible = [bool]($resolvedSecret -like 'native-secret-*')
$reportLines = @(
    "parentSecretVisible=$($parentSecretVisible.ToString().ToLowerInvariant())"
    "explicitEnvironmentVisible=$($explicitEnvironmentVisible.ToString().ToLowerInvariant())"
    "resolvedSecretVisible=$($resolvedSecretVisible.ToString().ToLowerInvariant())"
    'stderrMarkerEmitted=true'
)
[System.IO.File]::WriteAllLines($report, $reportLines, $utf8NoBom)

# The Java transport redirects stderr to DISCARD. This deliberately emits one very large line so
# the smoke test proves the SDK stderr reader is not retaining attacker-controlled unbounded data.
if ($stderrBytes -gt 0) {
    $chunk = 'x' * 8192
    $remaining = $stderrBytes
    while ($remaining -gt 0) {
        $count = [Math]::Min($remaining, $chunk.Length)
        [Console]::Error.Write($chunk.Substring(0, $count))
        $remaining -= $count
    }
    [Console]::Error.Flush()
}

while ($null -ne ($line = [Console]::In.ReadLine())) {
    if ([string]::IsNullOrWhiteSpace($line)) {
        continue
    }
    $request = $line | ConvertFrom-Json
    if ($null -eq $request.id) {
        continue
    }
    $response = [ordered]@{ jsonrpc = '2.0'; id = $request.id }
    switch ($request.method) {
        'initialize' {
            $response.result = [ordered]@{
                protocolVersion = '2025-03-26'
                capabilities = [ordered]@{ tools = [ordered]@{} }
                serverInfo = [ordered]@{ name = 'ja-native-stdio'; version = '1' }
            }
        }
        'tools/list' {
            $response.result = [ordered]@{
                tools = @([ordered]@{
                    name = 'echo'
                    description = 'Native stdio echo'
                    inputSchema = [ordered]@{ type = 'object'; properties = [ordered]@{} }
                })
            }
        }
        'tools/call' {
            $response.result = [ordered]@{
                content = @([ordered]@{ type = 'text'; text = 'native-stdio-ok' })
                isError = $false
            }
        }
        'ping' {
            $response.result = [ordered]@{}
        }
        default {
            $response.error = [ordered]@{ code = -32601; message = 'unknown' }
        }
    }
    $response | ConvertTo-Json -Compress -Depth 12
}
