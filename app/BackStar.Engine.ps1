# BackStar.Engine.ps1 - log parsing, job-queue backup engine, UI-state helpers, timers.

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
    # Under /MIR, an EXTRA file is about to be deleted - worth flagging in amber. Under /E
    # (incremental, non-destructive jobs), EXTRA just means "exists at the destination but not
    # in this source snapshot" - normal and harmless, so don't paint every incremental run amber.
    $mirrorRun = $true
    if ($script:CurrentJob -and $script:CurrentJob.Job -and ($null -ne $script:CurrentJob.Job.Mirror)) {
        $mirrorRun = [bool]$script:CurrentJob.Job.Mirror
    }
    foreach ($ln in $shown) {
        if ($ln -match '\bERROR\b') { Append-Log $ln 'Error' }
        elseif ($ln -match '\*EXTRA') { Append-Log $ln (if ($mirrorRun) { 'Warn' } else { 'Muted' }) }
        else { Append-Log $ln 'Muted' }
    }
    if ($lines.Count -gt $max) {
        Append-Log ("    ... +{0} more lines" -f ($lines.Count - $max)) 'Muted'
    }
    Trim-LogBox
}

function Set-UiEnabled([bool]$enabled) {
    # Both tabs share one job engine/queue, so everything that could start or reconfigure a run -
    # on EITHER tab, plus the tab switcher itself - is disabled while a backup is in progress.
    # Only the shared Start/Cancel button stays enabled, so cancelling always works.
    $btnAdd.Enabled            = $enabled
    $btnRemove.Enabled         = $enabled
    $btnBrowseDest.Enabled     = $enabled
    $lstSources.Enabled        = $enabled
    $chkGitGc.Enabled          = $enabled
    $btnTabProject.Enabled     = $enabled
    $btnTabSystem.Enabled      = $enabled
    $btnHistory.Enabled        = $enabled
    if ($lvCategories) {
        $lvCategories.Enabled         = $enabled
        $btnSelectAll.Enabled         = $enabled
        $btnRescanSizes.Enabled       = $enabled
        $lstSystemCustom.Enabled      = $enabled
        $btnAddCustom.Enabled         = $enabled
        $btnRemoveCustom.Enabled      = $enabled
        $btnBrowseSystemDest.Enabled  = $enabled
    }
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

function Get-RobocopyArgs {
    # Shared by every copy job (Project Backup, System Backup, Restore): only what varies between
    # them is parameterized. Mirror=true keeps today's /MIR (purges dest files no longer in source);
    # Mirror=false uses /E instead - plain recursive copy, robocopy's own timestamp/size compare
    # still skips unchanged files, but nothing at the destination is ever deleted.
    param(
        [string]$Source,
        [string]$Dest,
        [bool]$Mirror,
        [string[]]$ExcludeDirNames = @(),
        [string[]]$ExcludeDirPaths = @(),
        [string[]]$ExcludeFilePatterns = @()
    )
    $xd = ''
    if ($ExcludeDirNames.Count -gt 0 -or $ExcludeDirPaths.Count -gt 0) {
        $parts = New-Object System.Collections.Generic.List[string]
        foreach ($n in $ExcludeDirNames) { $parts.Add($n) }
        foreach ($rel in $ExcludeDirPaths) {
            # Full paths go to /XD quoted; robocopy matches them against the source tree exactly.
            $parts.Add("`"$(Join-Path $Source $rel)`"")
        }
        $xd = ' /XD ' + ($parts -join ' ')
    }
    $xf = ''
    if ($ExcludeFilePatterns.Count -gt 0) {
        $xf = ' /XF ' + ($ExcludeFilePatterns -join ' ')
    }
    $mirrorFlag = if ($Mirror) { '/MIR' } else { '/E' }
    # /FFT: 2-second timestamp tolerance so exFAT/FAT32 USB targets don't recopy everything each run.
    # /DST: tolerate the 1-hour daylight-saving skew. /NDL: skip directory lines (files show full paths).
    # Per-file progress stays ON (no /NP) so the UI can show live % while big files copy to slow USB targets.
    return "`"$Source`" `"$Dest`" $mirrorFlag$xd$xf /XJ /FFT /DST /NDL /R:2 /W:2 /MT:8"
}

function New-CopyJob {
    param(
        [string]$Name,
        [string]$Source,
        [string]$Dest,
        [bool]$Mirror,
        [string]$BackupProfile = 'Project',
        [string[]]$ExcludeDirNames = @(),
        [string[]]$ExcludeDirPaths = @(),
        [string[]]$ExcludeFilePatterns = @()
    )
    return [PSCustomObject]@{
        Name                = $Name
        Source              = $Source
        Dest                = $Dest
        Kind                = 'copy'
        Mirror              = $Mirror
        BackupProfile       = $BackupProfile
        ExcludeDirNames     = $ExcludeDirNames
        ExcludeDirPaths     = $ExcludeDirPaths
        ExcludeFilePatterns = $ExcludeFilePatterns
    }
}

function New-GcJob([string]$Name, [string]$Source, [string]$BackupProfile = 'Project') {
    return [PSCustomObject]@{ Name = $Name; Source = $Source; Dest = $null; Kind = 'gc'; Mirror = $true; BackupProfile = $BackupProfile }
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
        $exePath = 'robocopy.exe'
        $argStr = Get-RobocopyArgs -Source $job.Source -Dest $job.Dest -Mirror $job.Mirror `
            -ExcludeDirNames $job.ExcludeDirNames -ExcludeDirPaths $job.ExcludeDirPaths -ExcludeFilePatterns $job.ExcludeFilePatterns
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

    $durationSeconds = if ($script:RunStart) { [int]((Get-Date) - $script:RunStart).TotalSeconds } else { 0 }

    if ($script:Cancelled) {
        Append-Log ''
        Append-Log 'Backup cancelled.' 'Warn'
        Add-HistoryEntry -BackupProfile $script:RunProfile -StartedAt $script:RunStart -DurationSeconds $durationSeconds `
            -FilesCopied $script:TotalFilesCopied -OkCount $script:DoneJobs -FailCount 0 -Destination $script:RunDestination -Result 'Cancelled'
        # A dialog owned by a hidden/minimized-to-tray parent is a trap the user can't easily
        # get back to - skip it and rely on the balloon instead when the window isn't visible.
        if ($form.Visible) {
            Show-ThemedDialog -Message 'Backup was cancelled.' -Title 'BackStar' -Buttons OK -Kind Warn | Out-Null
        }
        else {
            Show-BackupNotification 'BackStar' 'Backup was cancelled.' 'Warn'
        }
        return
    }

    $okCount = ($script:ResultsSummary | Where-Object { $_ -like 'OK *' -and $_ -notlike 'OK gc:*' }).Count
    $failCount = ($script:ResultsSummary | Where-Object { $_ -like 'FAIL *' }).Count
    Append-Log ''
    Append-Log "===== Backup complete: $okCount succeeded, $failCount failed, $($script:TotalFilesCopied) file(s) copied =====" 'Header'

    Add-HistoryEntry -BackupProfile $script:RunProfile -StartedAt $script:RunStart -DurationSeconds $durationSeconds `
        -FilesCopied $script:TotalFilesCopied -OkCount $okCount -FailCount $failCount -Destination $script:RunDestination `
        -Result $(if ($failCount -gt 0) { 'Failed' } else { 'OK' })

    $summaryMsg = "$okCount of $($script:TotalCopyJobs) folder(s) backed up successfully.`n$($script:TotalFilesCopied) file(s) copied or updated."
    $kind = 'Info'
    if ($failCount -gt 0) {
        $summaryMsg += "`n$failCount failed - check the log for details."
        $kind = 'Error'
    }

    if ($form.Visible) {
        Show-ThemedDialog -Message $summaryMsg -Title 'BackStar - Backup Complete' -Buttons OK -Kind $kind | Out-Null
    }
    else {
        Show-BackupNotification 'BackStar - Backup Complete' $summaryMsg $kind
    }
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

