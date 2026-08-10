# Test harness: pulls Read-NewLogText, Update-ProgressFromTail and Process-LogChunk out of
# BackStar.ps1 via AST, stubs the UI bits they touch, then simulates timer ticks against a
# live robocopy run to verify progress parsing end to end.
$ErrorActionPreference = 'Stop'

$ast = [System.Management.Automation.Language.Parser]::ParseFile('E:\BackStar\BackStar.ps1', [ref]$null, [ref]$null)
$wanted = 'Read-NewLogText', 'Update-ProgressFromTail', 'Process-LogChunk'
foreach ($name in $wanted) {
    $fn = $ast.Find({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name }, $false)
    if (-not $fn) { throw "function $name not found" }
    Invoke-Expression $fn.Extent.Text
}

# --- stubs for UI dependencies ---
$script:FilesCopied = 0
$script:TotalFilesCopied = 0
$script:CurrentFile = ''
$script:CurrentPct = ''
$script:Logged = New-Object System.Collections.Generic.List[string]
function Append-Log([string]$Text, [string]$Level = 'Info') { $script:Logged.Add($Text) }
function Trim-LogBox { }

# --- live robocopy run into a log file (progress ON, like the patched script does) ---
$src = 'D:\LifeRise\apps\api'
$dst = Join-Path $env:TEMP ('BackStarTest_' + [System.Guid]::NewGuid().ToString('N'))
$log = Join-Path $env:TEMP ('BackStarTest_' + [System.Guid]::NewGuid().ToString('N') + '.log')
New-Item -ItemType Directory -Path $dst -Force | Out-Null
$proc = Start-Process -FilePath 'robocopy.exe' -ArgumentList "`"$src`" `"$dst`" admin-api.exe dbcheck.exe /NDL /R:1 /W:1 /MT:8" `
    -WindowStyle Hidden -PassThru -RedirectStandardOutput $log
$null = $proc.Handle

$offset = [long]0
$sawPct = $false
$sawFile = $false
$ticks = 0
while (-not $proc.HasExited -and $ticks -lt 3000) {
    $r = Read-NewLogText $log $offset
    if ($r.Text) { Process-LogChunk $r.Text }
    $offset = $r.Offset
    Update-ProgressFromTail $r.Tail
    if ($script:CurrentPct -ne '') { $sawPct = $true }
    if ($script:CurrentFile -match 'admin-api\.exe$') { $sawFile = $true }
    $ticks++
    Start-Sleep -Milliseconds 50
}
# drain
for ($i = 0; $i -lt 50; $i++) {
    $r = Read-NewLogText $log $offset $true
    if (-not $r.Text) { break }
    Process-LogChunk $r.Text
    $offset = $r.Offset
}

"exit code      : $($proc.ExitCode)"
"ticks          : $ticks"
"files counted  : $($script:FilesCopied) (expect 2)"
"saw live pct   : $sawPct"
"saw cur file   : $sawFile (last: $($script:CurrentFile))"
$progressSpam = @($script:Logged | Where-Object { $_ -match '\d+%' })
"progress lines leaked into UI log: $($progressSpam.Count) (expect 0)"
"copied-file lines in UI log:"
$script:Logged | Where-Object { $_ -match 'New File|Newer|Older' } | ForEach-Object { "  $_" }
Remove-Item -LiteralPath $dst -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $log -Force -ErrorAction SilentlyContinue
