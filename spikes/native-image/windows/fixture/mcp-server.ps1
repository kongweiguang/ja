# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

$ErrorActionPreference = 'Stop'

function Read-JsonRpcMessage {
    # AgentScope 2.0.2 的 MCP SDK 使用 UTF-8 JSONL；逐行读取可与客户端 BufferedReader 完全对齐。
    $line = [Console]::In.ReadLine()
    if ($null -eq $line) { return $null }
    return $line | ConvertFrom-Json
}

function Write-JsonRpcMessage([object]$message) {
    # 强制 flush 让客户端可以在同一轮请求中观察真实的 JSONL 响应边界。
    $json = $message | ConvertTo-Json -Compress -Depth 20
    [Console]::Out.WriteLine($json)
    [Console]::Out.Flush()
}

while ($true) {
    $request = Read-JsonRpcMessage
    if ($null -eq $request) { break }
    $method = [string]$request.method
    $id = $request.id
    switch ($method) {
        'initialize' {
            Write-JsonRpcMessage @{ jsonrpc = '2.0'; id = $id; result = @{ protocolVersion = '2024-11-05'; capabilities = @{ tools = @{} }; serverInfo = @{ name = 'ja-native-probe'; version = '1.0.0' } } }
        }
        'notifications/initialized' { }
        'tools/list' {
            Write-JsonRpcMessage @{ jsonrpc = '2.0'; id = $id; result = @{ tools = @(@{ name = 'probe_echo'; description = 'Deterministic local probe tool'; inputSchema = @{ type = 'object'; properties = @{ text = @{ type = 'string' } }; required = @('text') } }) } }
        }
        'tools/call' {
            $text = [string]$request.params.arguments.text
            Write-JsonRpcMessage @{ jsonrpc = '2.0'; id = $id; result = @{ content = @(@{ type = 'text'; text = "probe:$text" }); isError = $false } }
        }
        'ping' {
            Write-JsonRpcMessage @{ jsonrpc = '2.0'; id = $id; result = @{} }
        }
        default {
            Write-JsonRpcMessage @{ jsonrpc = '2.0'; id = $id; error = @{ code = -32601; message = "unsupported:$method" } }
        }
    }
}
