# Usage Monitor: reads Codex and Claude Code token usage on this machine and
# prints it summed by half hour (UTC) and model. Sent over SSH and run by
# Windows PowerShell; nothing is installed. Follows the same rules as the
# app's own reader (crates/core/src/cost/scan.rs). A small state file under
# %LOCALAPPDATA%\UsageMonitor lets the next run read only what the logs added.
$ErrorActionPreference = 'Stop'
$Version = 1
# Bumped when the state file's shape or what it keeps changes.
$CacheVersion = 2
$SlotSeconds = 1800
$CodexLongContextTokens = 272000
$CodexPrefixBytes = 192

$userHome = $env:USERPROFILE
$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $userHome '.codex' }
$codexRoots = @((Join-Path $codexHome 'sessions'), (Join-Path $codexHome 'archived_sessions'))
if ($env:CLAUDE_CONFIG_DIR -and $env:CLAUDE_CONFIG_DIR.Trim()) {
    $claudeRoots = @($env:CLAUDE_CONFIG_DIR.Replace(';', ',').Split(',') |
        Where-Object { $_.Trim() } | ForEach-Object { Join-Path $_.Trim() 'projects' })
} else {
    $claudeRoots = @((Join-Path $userHome '.claude\projects'), (Join-Path $userHome '.config\claude\projects'))
}
$cacheDir = Join-Path $env:LOCALAPPDATA 'UsageMonitor'
$cachePath = Join-Path $cacheDir 'remote-scan.txt'
$utf8 = New-Object System.Text.UTF8Encoding($false)
$invariant = [Globalization.CultureInfo]::InvariantCulture

# Claude lines can be megabytes long, so their fields are found by pattern
# rather than parsed whole. A key preceded by a backslash sits inside a
# string, not in the line's own structure.
$patternModel = New-Object Regex '(?<!\\)"model":"([^"\\]*)"'
$patternMessageId = New-Object Regex '(?<!\\)"id":"(msg_[^"\\]*)"'
$patternRequestId = New-Object Regex '(?<!\\)"requestId":"([^"\\]*)"'
$patternTimestamp = New-Object Regex '(?<!\\)"timestamp":"([^"\\]*)"', 'RightToLeft'
$usageFields = @{}
foreach ($name in 'input_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens', 'output_tokens', 'ephemeral_1h_input_tokens') {
    $usageFields[$name] = New-Object Regex ('"' + $name + '":(\d+)')
}

function Get-Slot($timestamp) {
    if ($null -eq $timestamp) { return $null }
    try {
        if ($timestamp -is [datetime]) {
            $seconds = ([DateTimeOffset]$timestamp.ToUniversalTime()).ToUnixTimeSeconds()
        } else {
            $seconds = [DateTimeOffset]::Parse([string]$timestamp, $invariant).ToUnixTimeSeconds()
        }
    } catch { return $null }
    return [int64]([Math]::Floor($seconds / $SlotSeconds) * $SlotSeconds)
}

function Get-Count($value) {
    if ($null -eq $value) { return [int64]0 }
    try { $number = [int64]$value } catch { return [int64]0 }
    if ($number -gt 0) { return $number }
    return [int64]0
}

function New-Entry($tool) {
    @{ tool = $tool; size = [int64]0; mtime = [int64]0; offset = [int64]0; model = $null
       lastTotal = $null; rows = @{}; claude = (New-Object System.Collections.ArrayList) }
}

function Read-CodexLine([string]$text, $entry) {
    try { $parsed = $text | ConvertFrom-Json } catch { return }
    $payload = $parsed.payload
    if ($null -eq $payload) { return }
    if ($parsed.type -eq 'turn_context') {
        if ($payload.model -and ([string]$payload.model).Trim()) { $entry.model = [string]$payload.model }
        return
    }
    if ($payload.type -ne 'token_count') { return }
    $info = $payload.info
    if ($null -eq $info) { return }
    $total = $null
    if ($null -ne $info.total_token_usage) {
        $total = Get-Count $info.total_token_usage.total_tokens
        if ($total -eq 0) {
            $total = (Get-Count $info.total_token_usage.input_tokens) + (Get-Count $info.total_token_usage.output_tokens)
        }
    }
    if ($null -ne $total -and $total -eq $entry.lastTotal) { return }
    if ($null -ne $total) { $entry.lastTotal = $total }
    $last = $info.last_token_usage
    if ($null -eq $last) { return }
    $model = if ($info.model) { [string]$info.model } elseif ($payload.model) { [string]$payload.model } else { $null }
    if (-not $model -or -not $model.Trim()) { $model = if ($entry.model) { $entry.model } else { 'unknown' } }
    $slot = Get-Slot $parsed.timestamp
    if ($null -eq $slot) { return }
    $inputTokens = Get-Count $last.input_tokens
    $cacheRead = [Math]::Min((Get-Count $last.cached_input_tokens), $inputTokens)
    $cacheWrite = [Math]::Min((Get-Count $last.cache_write_input_tokens), $inputTokens - $cacheRead)
    $output = Get-Count $last.output_tokens
    $tokens = @(($inputTokens - $cacheRead - $cacheWrite), $cacheRead, $cacheWrite, [int64]0, $output,
        [Math]::Min((Get-Count $last.reasoning_output_tokens), $output))
    if (($tokens[0] + $tokens[1] + $tokens[2] + $tokens[4]) -eq 0) { return }
    $long = if ($inputTokens -gt $CodexLongContextTokens) { 1 } else { 0 }
    $key = '{0}|{1}|{2}' -f $slot, $model, $long
    $row = $entry.rows[$key]
    if ($null -eq $row) { $row = New-Object 'int64[]' 6; $entry.rows[$key] = $row }
    for ($index = 0; $index -lt 6; $index++) { $row[$index] += $tokens[$index] }
}

function Read-ClaudeLine([string]$text, $entry) {
    # The response's usage comes after its content: take the last one.
    $at = $text.Length
    $usageAt = -1
    while ($at -gt 0) {
        $found = $text.LastIndexOf('"usage":{', $at - 1, [StringComparison]::Ordinal)
        if ($found -lt 0) { break }
        if ($found -eq 0 -or $text[$found - 1] -ne '\') { $usageAt = $found; break }
        $at = $found
    }
    if ($usageAt -lt 0) { return }
    $modelMatch = $patternModel.Match($text)
    if (-not $modelMatch.Success) { return }
    $model = $modelMatch.Groups[1].Value
    if (-not $model.Trim() -or $model.Trim() -eq '<synthetic>') { return }
    $timestampMatch = $patternTimestamp.Match($text)
    if (-not $timestampMatch.Success) { return }
    $slot = Get-Slot $timestampMatch.Groups[1].Value
    if ($null -eq $slot) { return }
    $usage = $text.Substring($usageAt, [Math]::Min(1500, $text.Length - $usageAt))
    $count = @{}
    foreach ($name in $usageFields.Keys) {
        $match = $usageFields[$name].Match($usage)
        $count[$name] = if ($match.Success) { [int64]$match.Groups[1].Value } else { [int64]0 }
    }
    $cacheWrite = $count['cache_creation_input_tokens']
    $tokens = @($count['input_tokens'], $count['cache_read_input_tokens'], $cacheWrite,
        [Math]::Min($count['ephemeral_1h_input_tokens'], $cacheWrite), $count['output_tokens'], [int64]0)
    if (($tokens[0] + $tokens[1] + $tokens[2] + $tokens[4]) -eq 0) { return }
    $messageId = $patternMessageId.Match($text)
    $requestId = $patternRequestId.Match($text)
    if ($messageId.Success -and $requestId.Success) {
        $key = $messageId.Groups[1].Value + ':' + $requestId.Groups[1].Value
    } elseif ($messageId.Success) {
        $key = $messageId.Groups[1].Value
    } else {
        $key = $timestampMatch.Groups[1].Value + ':' + $utf8.GetByteCount($text)
    }
    [void]$entry.claude.Add(@($key, $slot, $model) + $tokens)
}

# Reads the complete lines after the entry's offset and returns the bytes
# read through the last of them, so a line still being written is read
# whole next time.
function Read-NewLines($path, $entry) {
    $stream = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try {
        [void]$stream.Seek($entry.offset, 'Begin')
        $buffer = New-Object byte[] 4194304
        $carry = New-Object IO.MemoryStream
        $consumed = [int64]0
        $codex = $entry.tool -eq 'codex'
        while (($read = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $start = 0
            while ($start -lt $read) {
                $end = [Array]::IndexOf($buffer, [byte]10, $start, $read - $start)
                if ($end -lt 0) {
                    $carry.Write($buffer, $start, $read - $start)
                    break
                }
                $length = $end - $start
                if ($carry.Length -gt 0) {
                    $carry.Write($buffer, $start, $length)
                    $bytes = $carry.ToArray()
                    $carry.SetLength(0)
                    $lineLength = $bytes.Length
                    if ($codex) {
                        $prefix = $utf8.GetString($bytes, 0, [Math]::Min($lineLength, $CodexPrefixBytes))
                        if ($prefix.Contains('"type":"token_count"') -or $prefix.Contains('"type":"turn_context"')) {
                            Read-CodexLine $utf8.GetString($bytes) $entry
                        }
                    } else {
                        $text = $utf8.GetString($bytes)
                        if ($text.Contains('"usage"')) { Read-ClaudeLine $text $entry }
                    }
                } else {
                    $lineLength = $length
                    if ($codex) {
                        $prefix = $utf8.GetString($buffer, $start, [Math]::Min($length, $CodexPrefixBytes))
                        if ($prefix.Contains('"type":"token_count"') -or $prefix.Contains('"type":"turn_context"')) {
                            Read-CodexLine $utf8.GetString($buffer, $start, $length) $entry
                        }
                    } else {
                        $text = $utf8.GetString($buffer, $start, $length)
                        if ($text.Contains('"usage"')) { Read-ClaudeLine $text $entry }
                    }
                }
                $consumed += $lineLength + 1
                $start = $end + 1
            }
        }
        return $consumed
    } finally {
        $stream.Dispose()
    }
}

function Read-Cache {
    $files = @{}
    if (-not (Test-Path -LiteralPath $cachePath)) { return $files }
    $entry = $null
    $first = $true
    foreach ($line in [IO.File]::ReadAllLines($cachePath, $utf8)) {
        $fields = $line.Split("`t")
        if ($first) {
            if ($fields[0] -ne 'V' -or $fields[1] -ne [string]$CacheVersion) { return @{} }
            $first = $false
            continue
        }
        switch ($fields[0]) {
            'F' {
                $entry = New-Entry $fields[2]
                $entry.size = [int64]$fields[3]; $entry.mtime = [int64]$fields[4]; $entry.offset = [int64]$fields[5]
                if ($fields[6]) { $entry.model = $fields[6] }
                if ($fields[7]) { $entry.lastTotal = [int64]$fields[7] }
                $files[$fields[1]] = $entry
            }
            'R' {
                $row = New-Object 'int64[]' 6
                for ($index = 0; $index -lt 6; $index++) { $row[$index] = [int64]$fields[$index + 2] }
                $entry.rows[$fields[1]] = $row
            }
            'C' {
                $record = @($fields[1], [int64]$fields[2], $fields[3])
                for ($index = 4; $index -lt 10; $index++) { $record += [int64]$fields[$index] }
                [void]$entry.claude.Add($record)
            }
        }
    }
    return $files
}

function Write-Cache($files) {
    $builder = New-Object Text.StringBuilder
    [void]$builder.Append("V`t$CacheVersion`n")
    foreach ($path in $files.Keys) {
        $entry = $files[$path]
        [void]$builder.Append(("F`t{0}`t{1}`t{2}`t{3}`t{4}`t{5}`t{6}`n" -f $path, $entry.tool, $entry.size,
            $entry.mtime, $entry.offset, $entry.model, $entry.lastTotal))
        foreach ($key in $entry.rows.Keys) {
            [void]$builder.Append("R`t$key`t" + ($entry.rows[$key] -join "`t") + "`n")
        }
        foreach ($record in $entry.claude) {
            [void]$builder.Append("C`t" + ($record -join "`t") + "`n")
        }
    }
    [void](New-Item -ItemType Directory -Force -Path $cacheDir)
    $temporary = $cachePath + '.tmp'
    [IO.File]::WriteAllText($temporary, $builder.ToString(), $utf8)
    Move-Item -LiteralPath $temporary -Destination $cachePath -Force
}

function ConvertTo-JsonString([string]$value) {
    $builder = New-Object Text.StringBuilder
    [void]$builder.Append('"')
    foreach ($character in $value.ToCharArray()) {
        $code = [int]$character
        if ($character -eq '"' -or $character -eq '\') { [void]$builder.Append('\').Append($character) }
        elseif ($code -lt 32 -or $code -gt 126) { [void]$builder.Append(('\u{0:x4}' -f $code)) }
        else { [void]$builder.Append($character) }
    }
    [void]$builder.Append('"')
    return $builder.ToString()
}

$previous = @{}
try { $previous = Read-Cache } catch { $previous = @{} }
$files = @{}
$found = New-Object System.Collections.ArrayList
foreach ($pair in @(@('codex', $codexRoots), @('claude', $claudeRoots))) {
    $tool = $pair[0]
    foreach ($root in $pair[1]) {
        if (-not (Test-Path -LiteralPath $root -PathType Container)) { continue }
        if (-not $found.Contains($tool)) { [void]$found.Add($tool) }
        $list = @(Get-ChildItem -LiteralPath $root -Recurse -File -Filter '*.jsonl' -ErrorAction SilentlyContinue)
        foreach ($file in $list) {
            $mtime = ([DateTimeOffset]$file.LastWriteTimeUtc).ToUnixTimeSeconds()
            $path = $file.FullName
            $entry = $previous[$path]
            if ($entry -and $entry.tool -eq $tool -and $entry.size -eq $file.Length -and $entry.mtime -eq $mtime) {
                $files[$path] = $entry
                continue
            }
            if (-not $entry -or $entry.tool -ne $tool -or $entry.offset -gt $file.Length) { $entry = New-Entry $tool }
            try { $entry.offset += Read-NewLines $path $entry } catch { continue }
            $entry.size = $file.Length
            $entry.mtime = $mtime
            $files[$path] = $entry
        }
    }
}

try { Write-Cache $files } catch { }

$totals = @{}
$responses = @{}
foreach ($entry in $files.Values) {
    foreach ($key in $entry.rows.Keys) {
        $totalKey = 'codex|' + $key
        $total = $totals[$totalKey]
        if ($null -eq $total) { $total = New-Object 'int64[]' 6; $totals[$totalKey] = $total }
        $row = $entry.rows[$key]
        for ($index = 0; $index -lt 6; $index++) { $total[$index] += $row[$index] }
    }
    foreach ($record in $entry.claude) {
        $kept = $responses[$record[0]]
        # A response written twice keeps its fuller usage.
        if ($null -eq $kept -or $record[7] -gt $kept[7]) { $responses[$record[0]] = $record }
    }
}
foreach ($record in $responses.Values) {
    $totalKey = 'claude|{0}|{1}|0' -f $record[1], $record[2]
    $total = $totals[$totalKey]
    if ($null -eq $total) { $total = New-Object 'int64[]' 6; $totals[$totalKey] = $total }
    for ($index = 0; $index -lt 6; $index++) { $total[$index] += $record[$index + 3] }
}

$out = New-Object Text.StringBuilder
[void]$out.Append('{"usage_monitor":' + $Version + ',"found":[')
[void]$out.Append((($found | ForEach-Object { '"' + $_ + '"' }) -join ','))
[void]$out.Append('],"rows":[')
$firstRow = $true
foreach ($key in $totals.Keys) {
    $parts = $key.Split('|')
    $tool = $parts[0]
    $slot = $parts[1]
    $long = $parts[$parts.Length - 1]
    $model = ($parts[2..($parts.Length - 2)]) -join '|'
    if (-not $firstRow) { [void]$out.Append(',') }
    $firstRow = $false
    [void]$out.Append('["' + $tool + '",' + $slot + ',' + (ConvertTo-JsonString $model) + ',' + $long + ',' +
        ($totals[$key] -join ',') + ']')
}
[void]$out.Append(']}')
[Console]::Out.WriteLine($out.ToString())
