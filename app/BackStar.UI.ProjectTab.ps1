# BackStar.UI.ProjectTab.ps1 - main form shell, Project Backup controls and event wiring.

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
