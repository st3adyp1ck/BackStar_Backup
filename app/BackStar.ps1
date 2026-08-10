#requires -version 5.0
# BackStar - portable USB project backup tool.
# Mirrors selected source project folders into a destination folder using robocopy /MIR,
# skipping node_modules, common build/venv junk, and Capacitor-bundled web exports.
# Remembers last-used folders next to this script.

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

Add-Type -Language CSharp -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class BackStarNative {
    [DllImport("dwmapi.dll")]
    public static extern int DwmSetWindowAttribute(IntPtr hwnd, int attr, ref uint attrValue, int attrSize);
    [DllImport("uxtheme.dll", CharSet = CharSet.Unicode)]
    public static extern int SetWindowTheme(IntPtr hWnd, string pszSubAppName, string pszSubIdList);
}
"@

$script:ExcludeDirs  = @('node_modules', 'dist', 'build', '.next', '__pycache__', '.venv', 'venv', 'target', '.gradle', '.dart_tool', '.expo')
# Source-relative dirs excluded by full path (matched per source at job start), because their
# folder name alone is too common to ban globally - e.g. Capacitor-bundled web build exports
# that 'npx cap sync' regenerates, both named 'public' like real source asset folders.
$script:ExcludePaths = @(
    'apps\mobile\ios\App\App\public',
    'apps\mobile\android\app\src\main\assets\public'
)
$script:ConfigPath   = Join-Path $PSScriptRoot 'BackStar.config.json'
$script:LogoPath     = Join-Path $PSScriptRoot 'assets\BackStar-logo.png'
$script:MissingTag   = '[missing] '
$script:Running      = $false
$script:Cancelled    = $false
$script:CurrentJob   = $null
$script:JobQueue     = New-Object System.Collections.Generic.Queue[object]
$script:TotalJobs    = 0
$script:TotalCopyJobs = 0          # copy jobs only (gc jobs excluded), for the final summary
$script:DoneJobs     = 0
$script:ResultsSummary = New-Object System.Collections.Generic.List[string]
$script:BarState     = 'idle'      # idle | running | done | cancelled
$script:AnimPhase    = 0
$script:CurrentName  = ''
$script:CurrentKind  = 'copy'      # kind of the job currently running: 'copy' | 'gc'
$script:FilesCopied  = 0           # files copied within the current folder
$script:TotalFilesCopied = 0       # files copied across the whole run
$script:JobStart     = $null
$script:CurrentFile  = ''          # file robocopy is currently copying (leaf shown in the bar)
$script:CurrentPct   = ''          # live per-file progress, e.g. ' 47%' (empty when unknown)

# ---------- theme ----------

$Theme = @{
    BgMain      = [System.Drawing.Color]::FromArgb(10, 10, 15)
    BgPanel     = [System.Drawing.Color]::FromArgb(19, 21, 28)
    BarFill     = [System.Drawing.Color]::FromArgb(0, 110, 160)
    AccentBlue  = [System.Drawing.Color]::FromArgb(0, 200, 255)
    AccentRed   = [System.Drawing.Color]::FromArgb(232, 33, 60)
    AccentAmber = [System.Drawing.Color]::FromArgb(255, 176, 32)
    TextPrimary = [System.Drawing.Color]::FromArgb(230, 238, 245)
    TextMuted   = [System.Drawing.Color]::FromArgb(120, 132, 150)
}
$Theme.FontRegular  = New-Object System.Drawing.Font('Consolas', 9.5)
$Theme.FontBold     = New-Object System.Drawing.Font('Consolas', 9.5, [System.Drawing.FontStyle]::Bold)
$Theme.FontLog      = New-Object System.Drawing.Font('Consolas', 9)
$Theme.FontLogBold  = New-Object System.Drawing.Font('Consolas', 9, [System.Drawing.FontStyle]::Bold)
$Theme.FontHeading  = New-Object System.Drawing.Font('Consolas', 16, [System.Drawing.FontStyle]::Bold)
$Theme.FontSubtitle = New-Object System.Drawing.Font('Consolas', 8.5)
$Theme.FontBar      = New-Object System.Drawing.Font('Consolas', 8.5, [System.Drawing.FontStyle]::Bold)

# ---------- native theming helpers ----------

function ConvertTo-ColorRef([System.Drawing.Color]$c) {
    return [uint32]((([int]$c.B) -shl 16) -bor (([int]$c.G) -shl 8) -bor ([int]$c.R))
}

function Set-DarkTitleBar([System.Windows.Forms.Form]$targetForm) {
    try {
        $hwnd = $targetForm.Handle
        $enabled = [uint32]1
        [BackStarNative]::DwmSetWindowAttribute($hwnd, 20, [ref]$enabled, 4) | Out-Null   # DWMWA_USE_IMMERSIVE_DARK_MODE
        $caption = ConvertTo-ColorRef $Theme.BgMain
        [BackStarNative]::DwmSetWindowAttribute($hwnd, 35, [ref]$caption, 4) | Out-Null   # DWMWA_CAPTION_COLOR
        $text = ConvertTo-ColorRef $Theme.TextPrimary
        [BackStarNative]::DwmSetWindowAttribute($hwnd, 36, [ref]$text, 4) | Out-Null      # DWMWA_TEXT_COLOR
        $border = ConvertTo-ColorRef $Theme.AccentBlue
        [BackStarNative]::DwmSetWindowAttribute($hwnd, 34, [ref]$border, 4) | Out-Null    # DWMWA_BORDER_COLOR
    }
    catch { }
}

function Set-DarkScrollbars([System.Windows.Forms.Control]$ctrl) {
    try { [BackStarNative]::SetWindowTheme($ctrl.Handle, 'DarkMode_Explorer', $null) | Out-Null } catch { }
}

# ---------- UI element factories ----------

function Get-ShadedColor([System.Drawing.Color]$c, [int]$amount) {
    $r = [Math]::Max(0, [Math]::Min(255, $c.R + $amount))
    $g = [Math]::Max(0, [Math]::Min(255, $c.G + $amount))
    $b = [Math]::Max(0, [Math]::Min(255, $c.B + $amount))
    return [System.Drawing.Color]::FromArgb($r, $g, $b)
}

function New-ThemedButton([string]$text, [System.Drawing.Color]$bg, [System.Drawing.Color]$fg, [System.Drawing.Color]$border) {
    $b = New-Object System.Windows.Forms.Button
    $b.Text = $text
    $b.FlatStyle = 'Flat'
    $b.BackColor = $bg
    $b.ForeColor = $fg
    $b.Font = $Theme.FontBold
    $b.Cursor = [System.Windows.Forms.Cursors]::Hand
    $b.FlatAppearance.BorderColor = $border
    $b.FlatAppearance.BorderSize = 1
    $b.FlatAppearance.MouseOverBackColor = Get-ShadedColor $bg 25
    $b.FlatAppearance.MouseDownBackColor = Get-ShadedColor $bg -25
    return $b
}

function New-BorderPanel([System.Windows.Forms.Control]$inner, [System.Drawing.Color]$borderColor, [int]$pad = 2) {
    $wrap = New-Object System.Windows.Forms.Panel
    $wrap.BackColor = $borderColor
    $wrap.Padding = New-Object System.Windows.Forms.Padding($pad)
    $inner.Dock = 'Fill'
    $wrap.Controls.Add($inner)
    return $wrap
}

# ---------- themed dialog (replaces stock MessageBox) ----------

function Show-ThemedDialog {
    param(
        [string]$Message,
        [string]$Title = 'BackStar',
        [ValidateSet('OK', 'YesNo')] [string]$Buttons = 'OK',
        [ValidateSet('Info', 'Warn', 'Error')] [string]$Kind = 'Info'
    )
    $width = 440
    $textAreaWidth = $width - 40
    $flags = [System.Windows.Forms.TextFormatFlags]::WordBreak -bor [System.Windows.Forms.TextFormatFlags]::Left
    $textSize = [System.Windows.Forms.TextRenderer]::MeasureText($Message, $Theme.FontRegular, (New-Object System.Drawing.Size($textAreaWidth, 0)), $flags)
    $lblHeight = [Math]::Max(36, $textSize.Height + 8)
    $height = 24 + $lblHeight + 24 + 46

    $accent = switch ($Kind) { 'Error' { $Theme.AccentRed } 'Warn' { $Theme.AccentAmber } default { $Theme.AccentBlue } }

    $dlg = New-Object System.Windows.Forms.Form
    $dlg.Text = $Title
    $dlg.FormBorderStyle = 'FixedDialog'
    $dlg.MaximizeBox = $false
    $dlg.MinimizeBox = $false
    $dlg.ShowInTaskbar = $false
    $dlg.StartPosition = 'CenterParent'
    $dlg.BackColor = $Theme.BgMain
    $dlg.Font = $Theme.FontRegular
    $dlg.ClientSize = New-Object System.Drawing.Size($width, $height)

    $bar = New-Object System.Windows.Forms.Panel
    $bar.Size = New-Object System.Drawing.Size($width, 4)
    $bar.Location = New-Object System.Drawing.Point(0, 0)
    $bar.BackColor = $accent
    $dlg.Controls.Add($bar)

    $lbl = New-Object System.Windows.Forms.Label
    $lbl.Text = $Message
    $lbl.ForeColor = $Theme.TextPrimary
    $lbl.Location = New-Object System.Drawing.Point(20, 20)
    $lbl.Size = New-Object System.Drawing.Size($textAreaWidth, $lblHeight)
    $dlg.Controls.Add($lbl)

    $btnY = 20 + $lblHeight + 20

    if ($Buttons -eq 'YesNo') {
        $btnNo = New-ThemedButton 'No' $Theme.BgPanel $Theme.TextPrimary $Theme.TextMuted
        $btnNo.Size = New-Object System.Drawing.Size(90, 30)
        $btnNo.Location = New-Object System.Drawing.Point(($width - 110), $btnY)
        $btnNo.DialogResult = [System.Windows.Forms.DialogResult]::No
        $dlg.Controls.Add($btnNo)

        $btnYes = New-ThemedButton 'Yes' $accent $Theme.TextPrimary $accent
        $btnYes.Size = New-Object System.Drawing.Size(90, 30)
        $btnYes.Location = New-Object System.Drawing.Point(($width - 210), $btnY)
        $btnYes.DialogResult = [System.Windows.Forms.DialogResult]::Yes
        $dlg.Controls.Add($btnYes)

        $dlg.AcceptButton = $btnYes
        $dlg.CancelButton = $btnNo
    }
    else {
        $btnOk = New-ThemedButton 'OK' $accent $Theme.TextPrimary $accent
        $btnOk.Size = New-Object System.Drawing.Size(90, 30)
        $btnOk.Location = New-Object System.Drawing.Point(($width - 110), $btnY)
        $btnOk.DialogResult = [System.Windows.Forms.DialogResult]::OK
        $dlg.Controls.Add($btnOk)
        $dlg.AcceptButton = $btnOk
        $dlg.CancelButton = $btnOk
    }

    # $this = the dialog form itself; safer than capturing $dlg in the handler's scope
    $dlg.Add_Shown({ Set-DarkTitleBar $this })
    return $dlg.ShowDialog($script:form)
}

# ---------- helpers ----------

function Get-RawSourcePath([string]$item) {
    if ($item.StartsWith($script:MissingTag)) { return $item.Substring($script:MissingTag.Length) }
    return $item
}

function Format-SourceItem([string]$path) {
    if (Test-Path -LiteralPath $path -PathType Container) { return $path }
    return "$script:MissingTag$path"
}

function ConvertTo-StoredDestination([string]$destFull) {
    if ([string]::IsNullOrWhiteSpace($destFull)) { return $destFull }
    $rootFull = $PSScriptRoot.TrimEnd('\')
    if ($destFull.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) { return '.' }
    if ($destFull.StartsWith("$rootFull\", [System.StringComparison]::OrdinalIgnoreCase)) {
        return '.\' + $destFull.Substring($rootFull.Length + 1)
    }
    return $destFull
}

function Resolve-StoredDestination([string]$stored) {
    if ([string]::IsNullOrWhiteSpace($stored)) { return $stored }
    if ($stored.StartsWith('.\') -or $stored.StartsWith('./')) {
        return (Join-Path $PSScriptRoot $stored.Substring(2))
    }
    if ($stored -eq '.') { return $PSScriptRoot }
    return $stored
}

function Append-Log {
    param(
        [string]$Text,
        [ValidateSet('Info', 'Muted', 'Header', 'Success', 'Warn', 'Error')] [string]$Level = 'Info'
    )
    $color = switch ($Level) {
        'Header'  { $Theme.AccentBlue }
        'Success' { $Theme.AccentBlue }
        'Warn'    { $Theme.AccentAmber }
        'Error'   { $Theme.AccentRed }
        'Muted'   { $Theme.TextMuted }
        default   { $Theme.TextPrimary }
    }
    $txtLog.SelectionStart = $txtLog.TextLength
    $txtLog.SelectionLength = 0
    $txtLog.SelectionColor = $color
    $txtLog.SelectionFont = if ($Level -eq 'Header') { $Theme.FontLogBold } else { $Theme.FontLog }
    $txtLog.AppendText($Text + [Environment]::NewLine)
    $txtLog.ScrollToCaret()
}

function Trim-LogBox {
    # Keep the log box bounded so huge backups can't bog down the UI.
    if ($txtLog.TextLength -le 300000) { return }
    $line = $txtLog.GetLineFromCharIndex(120000)
    $idx = $txtLog.GetFirstCharIndexFromLine($line)
    if ($idx -le 0) { return }
    $txtLog.ReadOnly = $false
    $txtLog.Select(0, $idx)
    $txtLog.SelectedText = ''
    $txtLog.ReadOnly = $true
    $txtLog.SelectionStart = $txtLog.TextLength
}

function Read-NewLogText([string]$path, [long]$offset, [bool]$toEnd = $false) {
    # Reads newly appended text from robocopy's redirected output. Only consumes up to the
    # last complete line (unless $toEnd), so a line robocopy is mid-writing is never split.
    # Tail carries the unconsumed partial line: with per-file progress enabled, that is where
    # the live " 47%" updates for the file currently being copied accumulate.
    $result = [PSCustomObject]@{ Offset = $offset; Text = $null; Tail = $null }
    if (-not (Test-Path -LiteralPath $path)) { return $result }
    try {
        $fs = [System.IO.File]::Open($path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            if ($fs.Length -le $offset) { return $result }
            $fs.Seek($offset, [System.IO.SeekOrigin]::Begin) | Out-Null
            $len = [int]([Math]::Min($fs.Length - $offset, 1048576))
            $buf = New-Object byte[] ($len)
            $read = $fs.Read($buf, 0, $len)
            if ($read -le 0) { return $result }

            $take = $read
            if (-not $toEnd) {
                $lastNl = -1
                for ($i = $read - 1; $i -ge 0; $i--) {
                    if ($buf[$i] -eq 10) { $lastNl = $i; break }
                }
                if ($lastNl -lt 0) {
                    if ($read -lt 65536) {
                        # No complete line yet; surface the fragment for progress parsing.
                        $tailStart = [Math]::Max(0, $read - 8192)
                        $result.Tail = [System.Text.Encoding]::Default.GetString($buf, $tailStart, $read - $tailStart)
                        return $result
                    }
                }
                else {
                    $take = $lastNl + 1
                    $tailLen = $read - $take
                    if ($tailLen -gt 0) {
                        $tailStart = [Math]::Max($take, $read - 8192)
                        $result.Tail = [System.Text.Encoding]::Default.GetString($buf, $tailStart, $read - $tailStart)
                    }
                }
            }
            $result.Text = [System.Text.Encoding]::Default.GetString($buf, 0, $take)
            $result.Offset = $offset + $take
            return $result
        }
        finally {
            $fs.Dispose()
        }
    }
    catch {
        return $result
    }
}

function Update-ProgressFromTail([string]$tail) {
    # The tail is robocopy's half-written line. While a big file copies it looks like:
    # "\t  Older  \t\t 122.2 m\tD:\path\file.exe  0.8% 1.6% ..." (no trailing newline yet).
    if ([string]::IsNullOrEmpty($tail)) { return }
    $pctMatches = [regex]::Matches($tail, '\d+(?:\.\d+)?%')
    if ($pctMatches.Count -gt 0) {
        $script:CurrentPct = ' ' + $pctMatches[$pctMatches.Count - 1].Value
    }
    $tabIdx = $tail.LastIndexOf("`t")
    if ($tabIdx -ge 0 -and $tabIdx -lt $tail.Length -  1) {
        $candidate = $tail.Substring($tabIdx + 1)
        $pctIdx = $candidate.IndexOf('%')
        if ($pctIdx -ge 0) {
            # cut just before the first progress number preceding that '%'
            $candidate = $candidate.Substring(0, [Math]::Max(0, $candidate.LastIndexOf(' ', $pctIdx))).Trim()
        }
        else {
            $candidate = $candidate.Trim()
        }
        if ($candidate -match '^[A-Za-z]:\\' -and $candidate.Length -gt 3) {
            $script:CurrentFile = $candidate
        }
    }
}

function Process-LogChunk([string]$text) {
    $rawLines = @(($text -split "`r?`n") | Where-Object { $_.Trim() -ne '' })
    $lines = New-Object System.Collections.Generic.List[string]
    $copied = 0
    foreach ($ln in $rawLines) {
        if ($ln -match '^\s*[\d.%\s]+$') { continue }   # wrapped progress-only line: never show, never count
        $clean = $ln -replace '(\s+\d+(?:\.\d+)?%)+\s*$', ''   # strip trailing per-file progress tokens
        $lines.Add($clean)
        if ($clean -match '^\s*(New File|Newer|Older|Changed|Modified|Tweaked)\s') {
            $copied++
            $tabIdx = $clean.LastIndexOf("`t")
            if ($tabIdx -ge 0 -and $tabIdx -lt $clean.Length - 1) {
                $script:CurrentFile = $clean.Substring($tabIdx + 1).Trim()
            }
            $script:CurrentPct = ''
        }
    }
    $script:FilesCopied += $copied
    $script:TotalFilesCopied += $copied

    # Cap what hits the UI per tick; appending thousands of lines stalls the window.
    $max = 80
    $shown = if ($lines.Count -gt $max) { $lines.GetRange(0, $max) } else { $lines }
    foreach ($ln in $shown) {
        if ($ln -match '\bERROR\b') { Append-Log $ln 'Error' }
        elseif ($ln -match '\*EXTRA') { Append-Log $ln 'Warn' }
        else { Append-Log $ln 'Muted' }
    }
    if ($lines.Count -gt $max) {
        Append-Log ("    ... +{0} more lines" -f ($lines.Count - $max)) 'Muted'
    }
    Trim-LogBox
}

function Load-Config {
    if (Test-Path -LiteralPath $script:ConfigPath) {
        try {
            $cfg = Get-Content -LiteralPath $script:ConfigPath -Raw | ConvertFrom-Json
            if ($cfg.Sources) {
                foreach ($p in @($cfg.Sources)) {
                    if (-not [string]::IsNullOrWhiteSpace($p)) {
                        $lstSources.Items.Add((Format-SourceItem $p)) | Out-Null
                    }
                }
            }
            if ($cfg.Destination) {
                $txtDest.Text = Resolve-StoredDestination $cfg.Destination
            }
            if ($null -ne $cfg.GitGc) {
                $chkGitGc.Checked = [bool]$cfg.GitGc
            }
        }
        catch {
            Append-Log "Warning: could not read saved settings ($($_.Exception.Message))." 'Warn'
        }
    }
    if ([string]::IsNullOrWhiteSpace($txtDest.Text)) {
        $txtDest.Text = Join-Path $PSScriptRoot 'Backups'
    }
}

function Save-Config {
    $sources = @($lstSources.Items | ForEach-Object { Get-RawSourcePath $_ } | Where-Object { $_ })
    $cfg = [PSCustomObject]@{
        Destination = ConvertTo-StoredDestination $txtDest.Text.Trim()
        Sources     = $sources
        GitGc       = $chkGitGc.Checked
    }
    try {
        $cfg | ConvertTo-Json | Set-Content -LiteralPath $script:ConfigPath -Encoding UTF8
    }
    catch {
        Append-Log "Warning: could not save settings ($($_.Exception.Message))." 'Warn'
    }
}

function Set-UiEnabled([bool]$enabled) {
    $btnAdd.Enabled        = $enabled
    $btnRemove.Enabled     = $enabled
    $btnBrowseDest.Enabled = $enabled
    $lstSources.Enabled    = $enabled
    $chkGitGc.Enabled      = $enabled
}

function Set-StartButtonMode([bool]$isRunning) {
    if ($isRunning) {
        $btnStart.Text = 'Cancel'
        $btnStart.BackColor = $Theme.AccentAmber
        $btnStart.FlatAppearance.BorderColor = $Theme.AccentAmber
        $btnStart.FlatAppearance.MouseOverBackColor = Get-ShadedColor $Theme.AccentAmber 25
        $btnStart.FlatAppearance.MouseDownBackColor = Get-ShadedColor $Theme.AccentAmber -25
    }
    else {
        $btnStart.Text = 'Start Backup'
        $btnStart.BackColor = $Theme.AccentRed
        $btnStart.FlatAppearance.BorderColor = $Theme.AccentRed
        $btnStart.FlatAppearance.MouseOverBackColor = Get-ShadedColor $Theme.AccentRed 25
        $btnStart.FlatAppearance.MouseDownBackColor = Get-ShadedColor $Theme.AccentRed -25
    }
}

function Stop-CurrentProcess {
    if ($script:CurrentJob -and -not $script:CurrentJob.Process.HasExited) {
        try { $script:CurrentJob.Process.Kill() } catch { }
    }
}

function Start-NextJob {
    if ($script:Cancelled -or $script:JobQueue.Count -eq 0) {
        Finish-Run
        return
    }
    $job = $script:JobQueue.Dequeue()
    $logPath = Join-Path $env:TEMP ("BackStar_" + [System.Guid]::NewGuid().ToString('N') + '.log')
    $errLogPath = "$logPath.err"

    $script:CurrentName = $job.Name
    $script:CurrentKind = $job.Kind
    $script:FilesCopied = 0
    $script:CurrentFile = ''
    $script:CurrentPct = ''
    $script:JobStart = Get-Date

    if ($job.Kind -eq 'gc') {
        # Optional pre-backup step: let git decide whether the repo needs housekeeping
        # (--auto is a cheap no-op when thresholds aren't met).
        $exePath = 'git.exe'
        $argStr = "-C `"$($job.Source)`" gc --auto"
        $lblStatus.Text = "> git gc ($($script:DoneJobs + 1) of $($script:TotalJobs)): $($job.Name)"
        Append-Log ''
        Append-Log "===== git gc: $($job.Name) =====" 'Header'
        Append-Log "Repo: $($job.Source)" 'Muted'
    }
    else {
        $excludeStr = $script:ExcludeDirs -join ' '
        foreach ($rel in $script:ExcludePaths) {
            # Full paths go to /XD quoted; robocopy matches them against the source tree exactly.
            $excludeStr += " `"$(Join-Path $job.Source $rel)`""
        }
        # /FFT: 2-second timestamp tolerance so exFAT/FAT32 USB targets don't recopy everything each run.
        # /DST: tolerate the 1-hour daylight-saving skew. /NDL: skip directory lines (files show full paths).
        # Per-file progress stays ON (no /NP) so the UI can show live % while big files copy to slow USB targets.
        $exePath = 'robocopy.exe'
        $argStr = "`"$($job.Source)`" `"$($job.Dest)`" /MIR /XD $excludeStr /XJ /FFT /DST /NDL /R:2 /W:2 /MT:8"
        $lblStatus.Text = "> Backing up $($script:DoneJobs + 1) of $($script:TotalJobs): $($job.Name)"
        Append-Log ''
        Append-Log "===== $($job.Name) =====" 'Header'
        Append-Log "Source:      $($job.Source)" 'Muted'
        Append-Log "Destination: $($job.Dest)" 'Muted'
    }

    try {
        $proc = Start-Process -FilePath $exePath -ArgumentList $argStr -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput $logPath -RedirectStandardError $errLogPath
        # Force a real process handle onto the .NET object: without this, PS 5.1 reaps the
        # process before ExitCode is read and it silently returns $null (failures look like success).
        $null = $proc.Handle

        $script:CurrentJob = [PSCustomObject]@{
            Job        = $job
            Process    = $proc
            LogPath    = $logPath
            ErrLogPath = $errLogPath
            ReadOffset = [long]0
        }
    }
    catch {
        Append-Log "FAILED to start $($job.Kind) job for $($job.Name): $($_.Exception.Message)" 'Error'
        $script:ResultsSummary.Add("FAIL $($job.Name)")
        $script:DoneJobs++
        $barPanel.Invalidate()
        Start-NextJob
    }
}

function Finish-Run {
    $script:Timer.Stop()
    $script:AnimTimer.Stop()
    $script:Running = $false
    Set-UiEnabled $true
    Set-StartButtonMode $false
    $btnStart.Enabled = $true
    $lblStatus.Text = '> Ready.'
    $script:BarState = if ($script:Cancelled) { 'cancelled' } else { 'done' }
    $barPanel.Invalidate()

    if ($script:Cancelled) {
        Append-Log ''
        Append-Log 'Backup cancelled.' 'Warn'
        Show-ThemedDialog -Message 'Backup was cancelled.' -Title 'BackStar' -Buttons OK -Kind Warn | Out-Null
        return
    }

    $okCount = ($script:ResultsSummary | Where-Object { $_ -like 'OK *' -and $_ -notlike 'OK gc:*' }).Count
    $failCount = ($script:ResultsSummary | Where-Object { $_ -like 'FAIL *' }).Count
    Append-Log ''
    Append-Log "===== Backup complete: $okCount succeeded, $failCount failed, $($script:TotalFilesCopied) file(s) copied =====" 'Header'

    $summaryMsg = "$okCount of $($script:TotalCopyJobs) folder(s) backed up successfully.`n$($script:TotalFilesCopied) file(s) copied or updated."
    $kind = 'Info'
    if ($failCount -gt 0) {
        $summaryMsg += "`n$failCount failed - check the log for details."
        $kind = 'Error'
    }
    Show-ThemedDialog -Message $summaryMsg -Title 'BackStar - Backup Complete' -Buttons OK -Kind $kind | Out-Null
}

# ---------- form ----------

$form = New-Object System.Windows.Forms.Form
$form.Text = 'BackStar - Project Backup'
$form.ClientSize = New-Object System.Drawing.Size(694, 660)
$form.MinimumSize = New-Object System.Drawing.Size(600, 560)
$form.StartPosition = 'CenterScreen'
$form.BackColor = $Theme.BgMain
$form.Font = $Theme.FontRegular

# --- header: logo + title ---

$picLogo = New-Object System.Windows.Forms.PictureBox
$picLogo.SizeMode = 'Zoom'
$picLogo.BackColor = $Theme.BgMain
if (Test-Path -LiteralPath $script:LogoPath) {
    try { $picLogo.Image = [System.Drawing.Image]::FromFile($script:LogoPath) } catch { }
}
$pnlLogo = New-BorderPanel $picLogo $Theme.AccentBlue 2
$pnlLogo.Location = New-Object System.Drawing.Point(12, 12)
$pnlLogo.Size = New-Object System.Drawing.Size(60, 60)
$form.Controls.Add($pnlLogo)

$lblTitle = New-Object System.Windows.Forms.Label
$lblTitle.Text = 'BACKSTAR'
$lblTitle.Font = $Theme.FontHeading
$lblTitle.ForeColor = $Theme.TextPrimary
$lblTitle.BackColor = [System.Drawing.Color]::Transparent
$lblTitle.Location = New-Object System.Drawing.Point(84, 12)
$lblTitle.AutoSize = $true
$form.Controls.Add($lblTitle)

$lblSubtitle = New-Object System.Windows.Forms.Label
$lblSubtitle.Text = 'PROJECT BACKUP UTILITY'
$lblSubtitle.Font = $Theme.FontSubtitle
$lblSubtitle.ForeColor = $Theme.AccentRed
$lblSubtitle.BackColor = [System.Drawing.Color]::Transparent
$lblSubtitle.Location = New-Object System.Drawing.Point(86, 40)
$lblSubtitle.AutoSize = $true
$form.Controls.Add($lblSubtitle)

$sepGradient = New-Object System.Windows.Forms.Panel
$sepGradient.Location = New-Object System.Drawing.Point(12, 80)
$sepGradient.Size = New-Object System.Drawing.Size(670, 3)
$sepGradient.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$sepGradient.Add_Paint({
    param($s, $e)
    $rect = $s.ClientRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) { return }
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $Theme.AccentBlue, $Theme.AccentRed, 0.0)
    $e.Graphics.FillRectangle($brush, $rect)
    $brush.Dispose()
})
$form.Controls.Add($sepGradient)

# --- source folders ---

$lblSources = New-Object System.Windows.Forms.Label
$lblSources.Text = 'SOURCE FOLDERS'
$lblSources.Font = $Theme.FontBold
$lblSources.ForeColor = $Theme.AccentBlue
$lblSources.Location = New-Object System.Drawing.Point(12, 92)
$lblSources.AutoSize = $true
$form.Controls.Add($lblSources)

$lstSources = New-Object System.Windows.Forms.ListBox
$lstSources.BackColor = $Theme.BgPanel
$lstSources.ForeColor = $Theme.TextPrimary
$lstSources.BorderStyle = 'None'
$lstSources.SelectionMode = 'MultiExtended'
$lstSources.HorizontalScrollbar = $true
$lstSources.IntegralHeight = $false
$lstSources.Font = $Theme.FontRegular
$pnlSources = New-BorderPanel $lstSources $Theme.AccentBlue 2
$pnlSources.Location = New-Object System.Drawing.Point(12, 112)
$pnlSources.Size = New-Object System.Drawing.Size(550, 150)
$pnlSources.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($pnlSources)
Set-DarkScrollbars $lstSources

$btnAdd = New-ThemedButton 'Add Folder' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnAdd.Location = New-Object System.Drawing.Point(574, 112)
$btnAdd.Size = New-Object System.Drawing.Size(108, 34)
$btnAdd.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnAdd)

$btnRemove = New-ThemedButton 'Remove' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnRemove.Location = New-Object System.Drawing.Point(574, 150)
$btnRemove.Size = New-Object System.Drawing.Size(108, 34)
$btnRemove.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnRemove)

# --- destination ---

$lblDest = New-Object System.Windows.Forms.Label
$lblDest.Text = 'DESTINATION'
$lblDest.Font = $Theme.FontBold
$lblDest.ForeColor = $Theme.AccentBlue
$lblDest.Location = New-Object System.Drawing.Point(12, 272)
$lblDest.AutoSize = $true
$lblDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left
$form.Controls.Add($lblDest)

$txtDest = New-Object System.Windows.Forms.TextBox
$txtDest.BackColor = $Theme.BgPanel
$txtDest.ForeColor = $Theme.TextPrimary
$txtDest.BorderStyle = 'None'
$txtDest.ReadOnly = $true
$txtDest.Font = $Theme.FontRegular
$pnlDest = New-BorderPanel $txtDest $Theme.AccentBlue 2
$pnlDest.Location = New-Object System.Drawing.Point(12, 292)
$pnlDest.Size = New-Object System.Drawing.Size(550, 27)
$pnlDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($pnlDest)

$btnBrowseDest = New-ThemedButton 'Browse' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnBrowseDest.Location = New-Object System.Drawing.Point(574, 291)
$btnBrowseDest.Size = New-Object System.Drawing.Size(108, 29)
$btnBrowseDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnBrowseDest)

# --- git gc option ---

$chkGitGc = New-Object System.Windows.Forms.CheckBox
$chkGitGc.Text = "Run 'git gc --auto' on git repos before backing them up"
$chkGitGc.ForeColor = $Theme.TextMuted
$chkGitGc.BackColor = $Theme.BgMain
$chkGitGc.FlatStyle = 'Flat'
$chkGitGc.Font = $Theme.FontRegular
$chkGitGc.Location = New-Object System.Drawing.Point(12, 326)
$chkGitGc.Size = New-Object System.Drawing.Size(670, 22)
$chkGitGc.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$chkGitGc.Cursor = [System.Windows.Forms.Cursors]::Hand
# Checked state uses AccentRed so it's actually visible against the dark background (was TextMuted
# for both states, which made a checked box nearly impossible to see at a glance).
$chkGitGc.Add_CheckedChanged({
    $chkGitGc.ForeColor = if ($chkGitGc.Checked) { $Theme.AccentRed } else { $Theme.TextMuted }
})
$chkGitGc.Add_MouseEnter({ $chkGitGc.BackColor = Get-ShadedColor $Theme.BgMain 12 })
$chkGitGc.Add_MouseLeave({ $chkGitGc.BackColor = $Theme.BgMain })
$form.Controls.Add($chkGitGc)

# --- start / cancel ---

$btnStart = New-ThemedButton 'Start Backup' $Theme.AccentRed $Theme.TextPrimary $Theme.AccentRed
$btnStart.Location = New-Object System.Drawing.Point(12, 354)
$btnStart.Size = New-Object System.Drawing.Size(670, 40)
$btnStart.Font = New-Object System.Drawing.Font('Consolas', 11, [System.Drawing.FontStyle]::Bold)
$btnStart.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnStart)

# --- progress bar (custom-drawn: per-folder fill + animated sweep + live file count) ---

$barPanel = New-Object System.Windows.Forms.Panel
$barPanel.Location = New-Object System.Drawing.Point(12, 404)
$barPanel.Size = New-Object System.Drawing.Size(670, 24)
$barPanel.BackColor = $Theme.BgPanel
$barPanel.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$dbProp = [System.Windows.Forms.Control].GetProperty('DoubleBuffered', ([System.Reflection.BindingFlags]::Instance -bor [System.Reflection.BindingFlags]::NonPublic))
$dbProp.SetValue($barPanel, $true, $null)
$barPanel.Add_Paint({
    param($s, $e)
    $g = $e.Graphics
    $w = $s.ClientSize.Width
    $h = $s.ClientSize.Height
    if ($w -le 2 -or $h -le 2) { return }

    $bgBrush = New-Object System.Drawing.SolidBrush($Theme.BgPanel)
    $g.FillRectangle($bgBrush, 0, 0, $w, $h)
    $bgBrush.Dispose()

    $fillW = 0
    if ($script:BarState -eq 'done') {
        $fillW = $w
    }
    elseif ($script:TotalJobs -gt 0) {
        $fillW = [int]($w * $script:DoneJobs / $script:TotalJobs)
    }
    if ($fillW -gt 0) {
        $fb = New-Object System.Drawing.SolidBrush($Theme.BarFill)
        $g.FillRectangle($fb, 0, 0, $fillW, $h)
        $fb.Dispose()
    }

    if ($script:BarState -eq 'running' -and $fillW -lt $w) {
        $regionW = $w - $fillW
        $bandW = 110
        $span = $regionW + $bandW
        $pos = $fillW + (([int]$script:AnimPhase) % $span) - $bandW
        $clip = New-Object System.Drawing.Rectangle($fillW, 0, $regionW, $h)
        $g.SetClip($clip)
        $bandRect = New-Object System.Drawing.Rectangle($pos, 0, $bandW, $h)
        $lg = New-Object System.Drawing.Drawing2D.LinearGradientBrush($bandRect, $Theme.AccentBlue, $Theme.AccentBlue, 0.0)
        $cb = New-Object System.Drawing.Drawing2D.ColorBlend(3)
        $cb.Colors = @(
            [System.Drawing.Color]::FromArgb(0, 0, 200, 255),
            [System.Drawing.Color]::FromArgb(120, 0, 200, 255),
            [System.Drawing.Color]::FromArgb(0, 0, 200, 255)
        )
        $cb.Positions = @([single]0.0, [single]0.5, [single]1.0)
        $lg.InterpolationColors = $cb
        $g.FillRectangle($lg, $bandRect)
        $lg.Dispose()
        $g.ResetClip()
    }

    $pen = New-Object System.Drawing.Pen($Theme.AccentBlue)
    $g.DrawRectangle($pen, 0, 0, $w - 1, $h - 1)
    $pen.Dispose()

    $text = ''
    $textColor = $Theme.TextMuted
    switch ($script:BarState) {
        'running'   {
            $elapsed = if ($script:JobStart) { ((Get-Date) - $script:JobStart).ToString('mm\:ss') } else { '00:00' }
            $queuePos = "[$([Math]::Min($script:DoneJobs + 1, $script:TotalJobs))/$($script:TotalJobs)]"
            if ($script:CurrentKind -eq 'gc') {
                $text = "$queuePos GIT GC $($script:CurrentName)  ::  $elapsed"
            }
            else {
                $fileBit = ''
                if ($script:CurrentFile) {
                    $leaf = Split-Path $script:CurrentFile -Leaf
                    $fileBit = "  ::  $leaf$($script:CurrentPct)"
                }
                $text = "$queuePos SYNCING $($script:CurrentName)  ::  $($script:FilesCopied) FILES$fileBit  ::  $elapsed"
            }
            $textColor = $Theme.TextPrimary
        }
        'done'      { $text = "COMPLETE  ::  $($script:TotalFilesCopied) FILES"; $textColor = $Theme.TextPrimary }
        'cancelled' { $text = 'CANCELLED'; $textColor = $Theme.AccentAmber }
        default     { $text = 'STANDBY' }
    }
    $tf = [System.Windows.Forms.TextFormatFlags]::HorizontalCenter -bor
          [System.Windows.Forms.TextFormatFlags]::VerticalCenter -bor
          [System.Windows.Forms.TextFormatFlags]::EndEllipsis
    [System.Windows.Forms.TextRenderer]::DrawText($g, $text, $Theme.FontBar, $s.ClientRectangle, $textColor, $tf)
})
$form.Controls.Add($barPanel)

$lblStatus = New-Object System.Windows.Forms.Label
$lblStatus.Text = '> Ready.'
$lblStatus.ForeColor = $Theme.TextMuted
$lblStatus.Font = $Theme.FontRegular
$lblStatus.Location = New-Object System.Drawing.Point(12, 434)
$lblStatus.Size = New-Object System.Drawing.Size(670, 18)
$lblStatus.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($lblStatus)

# --- log ---

$lblLog = New-Object System.Windows.Forms.Label
$lblLog.Text = 'ACTIVITY LOG'
$lblLog.Font = $Theme.FontBold
$lblLog.ForeColor = $Theme.AccentBlue
$lblLog.Location = New-Object System.Drawing.Point(12, 458)
$lblLog.AutoSize = $true
$form.Controls.Add($lblLog)

$txtLog = New-Object System.Windows.Forms.RichTextBox
$txtLog.BackColor = $Theme.BgPanel
$txtLog.ForeColor = $Theme.TextPrimary
$txtLog.BorderStyle = 'None'
$txtLog.ReadOnly = $true
$txtLog.WordWrap = $false
$txtLog.ScrollBars = 'ForcedBoth'
$txtLog.Font = $Theme.FontLog
$pnlLog = New-BorderPanel $txtLog $Theme.AccentBlue 2
$pnlLog.Location = New-Object System.Drawing.Point(12, 478)
$pnlLog.Size = New-Object System.Drawing.Size(670, 170)
$pnlLog.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right -bor [System.Windows.Forms.AnchorStyles]::Bottom
$form.Controls.Add($pnlLog)
Set-DarkScrollbars $txtLog

Set-StartButtonMode $false

# --- window icon (taskbar): prefer a real multi-size .ico built from the logo PNG ---

$script:IconPath = Join-Path $PSScriptRoot 'assets\BackStar.ico'
if (Test-Path -LiteralPath $script:IconPath) {
    try { $form.Icon = New-Object System.Drawing.Icon($script:IconPath) } catch { }
}
elseif (Test-Path -LiteralPath $script:LogoPath) {
    try {
        $iconBmp = New-Object System.Drawing.Bitmap($script:LogoPath)
        $form.Icon = [System.Drawing.Icon]::FromHandle($iconBmp.GetHicon())
    }
    catch { }
}

# ---------- timers ----------

$script:AnimTimer = New-Object System.Windows.Forms.Timer
$script:AnimTimer.Interval = 50
$script:AnimTimer.Add_Tick({
    $script:AnimPhase += 7
    if ($script:AnimPhase -gt 1000000000) { $script:AnimPhase = 0 }
    $barPanel.Invalidate()
})

$script:Timer = New-Object System.Windows.Forms.Timer
$script:Timer.Interval = 400
$script:Timer.Add_Tick({
    try {
        if (-not $script:CurrentJob) { return }

        $r = Read-NewLogText $script:CurrentJob.LogPath $script:CurrentJob.ReadOffset
        if ($r.Text) { Process-LogChunk $r.Text }
        $script:CurrentJob.ReadOffset = $r.Offset
        Update-ProgressFromTail $r.Tail

        $proc = $script:CurrentJob.Process
        if (-not $proc.HasExited) { return }

        # Drain whatever robocopy wrote between the last read and exit.
        for ($i = 0; $i -lt 50; $i++) {
            $r = Read-NewLogText $script:CurrentJob.LogPath $script:CurrentJob.ReadOffset $true
            if (-not $r.Text) { break }
            Process-LogChunk $r.Text
            $script:CurrentJob.ReadOffset = $r.Offset
        }
        $job = $script:CurrentJob.Job

        if ($script:Cancelled) {
            Append-Log "Cancelled during: $($job.Name)" 'Warn'
        }
        else {
            $code = $proc.ExitCode
            $elapsed = (Get-Date) - $script:JobStart
            $elapsedStr = if ($elapsed.TotalHours -ge 1) { $elapsed.ToString('hh\:mm\:ss') } else { $elapsed.ToString('mm\:ss') }
            if ($job.Kind -eq 'gc') {
                if ($code -eq 0) {
                    Append-Log "Done: git gc $($job.Name) ($elapsedStr)" 'Success'
                    $script:ResultsSummary.Add("OK gc:$($job.Name)")
                }
                else {
                    Append-Log "FAILED: git gc $($job.Name) (git exit code $code)" 'Error'
                    $script:ResultsSummary.Add("FAIL gc:$($job.Name)")
                }
            }
            elseif ($code -lt 8) {
                if ($code -eq 0) {
                    Append-Log "Done: $($job.Name) - no changes needed ($elapsedStr)" 'Success'
                }
                else {
                    Append-Log "Done: $($job.Name) - $($script:FilesCopied) file(s) in $elapsedStr" 'Success'
                }
                $script:ResultsSummary.Add("OK $($job.Name)")
            }
            else {
                Append-Log "FAILED: $($job.Name) (robocopy exit code $code)" 'Error'
                $script:ResultsSummary.Add("FAIL $($job.Name)")
            }
            $script:DoneJobs++
            $barPanel.Invalidate()
        }

        Remove-Item -LiteralPath $script:CurrentJob.LogPath -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $script:CurrentJob.ErrLogPath -ErrorAction SilentlyContinue
        $script:CurrentJob = $null

        if ($script:Cancelled) { Finish-Run } else { Start-NextJob }
    }
    catch {
        Append-Log "ERROR: $($_.Exception.Message)" 'Error'
        $script:Cancelled = $true
        Finish-Run
    }
})

# ---------- events ----------

$btnAdd.Add_Click({
    try {
        $fbd = New-Object System.Windows.Forms.FolderBrowserDialog
        $fbd.Description = 'Select a project folder to back up'
        $fbd.ShowNewFolderButton = $false
        if ($fbd.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            if ($fbd.SelectedPath -eq [System.IO.Path]::GetPathRoot($fbd.SelectedPath)) {
                Show-ThemedDialog -Message 'Please pick a folder, not a whole drive root.' -Buttons OK -Kind Warn | Out-Null
                return
            }
            $path = $fbd.SelectedPath.TrimEnd('\')
            $existingPaths = @($lstSources.Items | ForEach-Object { Get-RawSourcePath $_ })

            if ($existingPaths -contains $path) {
                Show-ThemedDialog -Message 'That folder is already in the list.' -Buttons OK -Kind Info | Out-Null
                return
            }

            $name = Split-Path $path -Leaf
            $existingNames = @($existingPaths | ForEach-Object { Split-Path $_ -Leaf })
            if ($existingNames -contains $name) {
                Show-ThemedDialog -Message "Another selected folder is already named '$name'. Each source needs a unique folder name, since that name becomes its backup subfolder." -Buttons OK -Kind Warn | Out-Null
                return
            }

            $lstSources.Items.Add((Format-SourceItem $path)) | Out-Null
        }
    }
    catch {
        Show-ThemedDialog -Message "Error adding folder: $($_.Exception.Message)" -Buttons OK -Kind Error | Out-Null
    }
})

$btnRemove.Add_Click({
    foreach ($item in @($lstSources.SelectedItems)) {
        $lstSources.Items.Remove($item)
    }
})

$btnBrowseDest.Add_Click({
    try {
        $fbd = New-Object System.Windows.Forms.FolderBrowserDialog
        $fbd.Description = 'Select the backup destination folder'
        $fbd.ShowNewFolderButton = $true
        if (-not [string]::IsNullOrWhiteSpace($txtDest.Text) -and (Test-Path -LiteralPath $txtDest.Text)) {
            $fbd.SelectedPath = $txtDest.Text
        }
        if ($fbd.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            $txtDest.Text = $fbd.SelectedPath.TrimEnd('\')
        }
    }
    catch {
        Show-ThemedDialog -Message "Error selecting destination: $($_.Exception.Message)" -Buttons OK -Kind Error | Out-Null
    }
})

$btnStart.Add_Click({
    if ($script:Running) {
        $script:Cancelled = $true
        Append-Log 'Cancelling...' 'Warn'
        $btnStart.Enabled = $false
        Stop-CurrentProcess
        return
    }

    try {
        $dest = $txtDest.Text.Trim()
        if ([string]::IsNullOrWhiteSpace($dest)) {
            Show-ThemedDialog -Message 'Please choose a destination folder first.' -Buttons OK -Kind Warn | Out-Null
            return
        }
        if (-not (Test-Path -LiteralPath $dest)) {
            try {
                New-Item -ItemType Directory -Path $dest -Force | Out-Null
            }
            catch {
                Show-ThemedDialog -Message "Could not create destination folder:`n$dest" -Buttons OK -Kind Error | Out-Null
                return
            }
        }
        $destFull = (Resolve-Path -LiteralPath $dest).Path.TrimEnd('\')

        $jobs = New-Object System.Collections.Generic.List[object]
        $skipped = New-Object System.Collections.Generic.List[string]

        foreach ($item in $lstSources.Items) {
            $raw = Get-RawSourcePath $item
            if (-not (Test-Path -LiteralPath $raw -PathType Container)) {
                $skipped.Add($raw)
                continue
            }
            $srcFull = (Resolve-Path -LiteralPath $raw).Path.TrimEnd('\')
            $name = Split-Path $srcFull -Leaf
            $destSub = Join-Path $destFull $name

            if ($destFull.Equals($srcFull, [System.StringComparison]::OrdinalIgnoreCase) -or
                $destFull.StartsWith("$srcFull\", [System.StringComparison]::OrdinalIgnoreCase) -or
                $srcFull.StartsWith("$destFull\", [System.StringComparison]::OrdinalIgnoreCase)) {
                Show-ThemedDialog -Message "The destination can't be the same as, inside, or contain a source folder:`n$srcFull" -Buttons OK -Kind Error | Out-Null
                return
            }

            $jobs.Add([PSCustomObject]@{ Name = $name; Source = $srcFull; Dest = $destSub; Kind = 'copy' })
        }

        if ($jobs.Count -eq 0) {
            Show-ThemedDialog -Message 'No valid source folders to back up. Add a folder first (or check that previously remembered folders still exist on this machine).' -Buttons OK -Kind Warn | Out-Null
            return
        }

        $msg = "This will mirror $($jobs.Count) folder(s) into:`n$destFull`n`nFiles or folders inside the destination that no longer exist in the matching source will be DELETED so the backup matches the source. Continue?"
        $confirm = Show-ThemedDialog -Message $msg -Title 'Confirm Backup' -Buttons YesNo -Kind Warn
        if ($confirm -ne [System.Windows.Forms.DialogResult]::Yes) { return }

        Save-Config

        $script:Running = $true
        $script:Cancelled = $false
        $script:ResultsSummary = New-Object System.Collections.Generic.List[string]
        $script:JobQueue = New-Object System.Collections.Generic.Queue[object]
        $gitAvailable = [bool](Get-Command git.exe -ErrorAction SilentlyContinue)
        $gitGcQueued = 0
        foreach ($j in $jobs) {
            if ($chkGitGc.Checked -and $gitAvailable -and
                (Test-Path -LiteralPath (Join-Path $j.Source '.git') -PathType Container)) {
                $script:JobQueue.Enqueue([PSCustomObject]@{ Name = $j.Name; Source = $j.Source; Dest = $null; Kind = 'gc' })
                $gitGcQueued++
            }
            $script:JobQueue.Enqueue($j)
        }
        $script:TotalJobs = $script:JobQueue.Count
        $script:TotalCopyJobs = $jobs.Count
        $script:DoneJobs = 0
        $script:TotalFilesCopied = 0
        $script:FilesCopied = 0
        $script:AnimPhase = 0
        $script:BarState = 'running'
        $barPanel.Invalidate()

        $txtLog.Clear()
        foreach ($s in $skipped) { Append-Log "Skipping (not found on this machine): $s" 'Warn' }
        if ($chkGitGc.Checked -and -not $gitAvailable) {
            Append-Log "git.exe not found on PATH - skipping the git gc step." 'Warn'
        }
        elseif ($gitGcQueued -gt 0) {
            Append-Log "git gc --auto queued for $gitGcQueued repo(s) before their backup." 'Muted'
        }

        Set-UiEnabled $false
        Set-StartButtonMode $true

        # Timers must start BEFORE the first job: if every job fails to launch, Start-NextJob
        # reaches Finish-Run synchronously, and Finish-Run must be the last thing to touch them.
        $script:Timer.Start()
        $script:AnimTimer.Start()
        Start-NextJob
    }
    catch {
        Show-ThemedDialog -Message "Unexpected error: $($_.Exception.Message)" -Buttons OK -Kind Error | Out-Null
        Set-UiEnabled $true
        Set-StartButtonMode $false
        $script:Running = $false
    }
})

$form.Add_FormClosing({
    param($s, $e)
    if ($script:Running) {
        $r = Show-ThemedDialog -Message 'A backup is still running. Stop it and exit?' -Title 'BackStar' -Buttons YesNo -Kind Warn
        if ($r -ne [System.Windows.Forms.DialogResult]::Yes) {
            $e.Cancel = $true
            return
        }
        $script:Cancelled = $true
        $script:Running = $false
        $script:Timer.Stop()
        $script:AnimTimer.Stop()
        Stop-CurrentProcess
        if ($script:CurrentJob) {
            Remove-Item -LiteralPath $script:CurrentJob.LogPath -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath $script:CurrentJob.ErrLogPath -ErrorAction SilentlyContinue
        }
    }
    Save-Config
})

$form.Add_Shown({
    $form.Activate()
    Set-DarkTitleBar $form
})

Load-Config

[System.Windows.Forms.Application]::Run($form)
