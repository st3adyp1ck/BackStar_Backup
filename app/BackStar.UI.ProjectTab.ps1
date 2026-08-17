# BackStar.UI.ProjectTab.ps1 - Project Backup tab controls (mirror-style, /MIR) and its Start logic.
# Populates $pnlProjectTab (built by the entry script). Coordinates below are relative to that
# panel's own client area, not the form.

# --- source folders ---

$lblSources = New-Object System.Windows.Forms.Label
$lblSources.Text = 'SOURCE FOLDERS'
$lblSources.Font = $Theme.FontBold
$lblSources.ForeColor = $Theme.AccentBlue
$lblSources.Location = New-Object System.Drawing.Point(0, 0)
$lblSources.AutoSize = $true
$pnlProjectTab.Controls.Add($lblSources)

$lstSources = New-Object System.Windows.Forms.ListBox
$lstSources.BackColor = $Theme.BgPanel
$lstSources.ForeColor = $Theme.TextPrimary
$lstSources.BorderStyle = 'None'
$lstSources.SelectionMode = 'MultiExtended'
$lstSources.HorizontalScrollbar = $true
$lstSources.IntegralHeight = $false
$lstSources.Font = $Theme.FontRegular
$pnlSources = New-BorderPanel $lstSources $Theme.AccentBlue 2
$pnlSources.Location = New-Object System.Drawing.Point(0, 20)
$pnlSources.Size = New-Object System.Drawing.Size(550, 155)
$pnlSources.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlProjectTab.Controls.Add($pnlSources)
Set-DarkScrollbars $lstSources

$btnAdd = New-ThemedButton 'Add Folder' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnAdd.Location = New-Object System.Drawing.Point(562, 20)
$btnAdd.Size = New-Object System.Drawing.Size(108, 34)
$btnAdd.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlProjectTab.Controls.Add($btnAdd)

$btnRemove = New-ThemedButton 'Remove' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnRemove.Location = New-Object System.Drawing.Point(562, 58)
$btnRemove.Size = New-Object System.Drawing.Size(108, 34)
$btnRemove.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlProjectTab.Controls.Add($btnRemove)

# --- destination ---

$lblDest = New-Object System.Windows.Forms.Label
$lblDest.Text = 'DESTINATION'
$lblDest.Font = $Theme.FontBold
$lblDest.ForeColor = $Theme.AccentBlue
$lblDest.Location = New-Object System.Drawing.Point(0, 183)
$lblDest.AutoSize = $true
$lblDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left
$pnlProjectTab.Controls.Add($lblDest)

$txtDest = New-Object System.Windows.Forms.TextBox
$txtDest.BackColor = $Theme.BgPanel
$txtDest.ForeColor = $Theme.TextPrimary
$txtDest.BorderStyle = 'None'
$txtDest.ReadOnly = $true
$txtDest.Font = $Theme.FontRegular
$pnlDest = New-BorderPanel $txtDest $Theme.AccentBlue 2
$pnlDest.Location = New-Object System.Drawing.Point(0, 203)
$pnlDest.Size = New-Object System.Drawing.Size(550, 27)
$pnlDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlProjectTab.Controls.Add($pnlDest)

$btnBrowseDest = New-ThemedButton 'Browse' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnBrowseDest.Location = New-Object System.Drawing.Point(562, 202)
$btnBrowseDest.Size = New-Object System.Drawing.Size(108, 29)
$btnBrowseDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlProjectTab.Controls.Add($btnBrowseDest)

# --- git gc option ---

$chkGitGc = New-Object System.Windows.Forms.CheckBox
$chkGitGc.Text = "Run 'git gc --auto' on git repos before backing them up"
$chkGitGc.ForeColor = $Theme.TextMuted
$chkGitGc.BackColor = $Theme.BgMain
$chkGitGc.FlatStyle = 'Flat'
$chkGitGc.Font = $Theme.FontRegular
$chkGitGc.Location = New-Object System.Drawing.Point(0, 236)
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
$pnlProjectTab.Controls.Add($chkGitGc)

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

function Test-DestNotInsideSource([string]$destFull, [string]$srcFull) {
    return -not ($destFull.Equals($srcFull, [System.StringComparison]::OrdinalIgnoreCase) -or
        $destFull.StartsWith("$srcFull\", [System.StringComparison]::OrdinalIgnoreCase) -or
        $srcFull.StartsWith("$destFull\", [System.StringComparison]::OrdinalIgnoreCase))
}

function Start-ProjectFullBackupRun {
    # Additive only: copies new/changed files, never deletes anything at the destination, even
    # files removed from the matching project folder. The "safe" default. Use Sync for a true
    # mirror that also deletes.
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

            if (-not (Test-DestNotInsideSource $destFull $srcFull)) {
                Show-ThemedDialog -Message "The destination can't be the same as, inside, or contain a source folder:`n$srcFull" -Buttons OK -Kind Error | Out-Null
                return
            }

            $jobs.Add((New-CopyJob -Name $name -Source $srcFull -Dest $destSub -Mirror $false -BackupProfile 'Project' `
                -ExcludeDirNames $script:ExcludeDirs -ExcludeDirPaths $script:ExcludePaths))
        }

        if ($jobs.Count -eq 0) {
            Show-ThemedDialog -Message 'No valid source folders to back up. Add a folder first (or check that previously remembered folders still exist on this machine).' -Buttons OK -Kind Warn | Out-Null
            return
        }

        $msg = "This will copy $($jobs.Count) folder(s) into:`n$destFull`n`nOnly new or changed files are copied. Nothing already at the destination is ever deleted. Continue?"
        $confirm = Show-ThemedDialog -Message $msg -Title 'Confirm Full Backup' -Buttons YesNo -Kind Info
        if ($confirm -ne [System.Windows.Forms.DialogResult]::Yes) { return }

        $gitAvailable = [bool](Get-Command git.exe -ErrorAction SilentlyContinue)
        $gitGcQueued = 0
        $queueJobs = New-Object System.Collections.Generic.List[object]
        foreach ($j in $jobs) {
            if ($chkGitGc.Checked -and $gitAvailable -and
                (Test-Path -LiteralPath (Join-Path $j.Source '.git') -PathType Container)) {
                $queueJobs.Add((New-GcJob -Name $j.Name -Source $j.Source -BackupProfile 'Project'))
                $gitGcQueued++
            }
            $queueJobs.Add($j)
        }

        $txtLog.Clear()
        foreach ($s in $skipped) { Append-Log "Skipping (not found on this machine): $s" 'Warn' }
        if ($chkGitGc.Checked -and -not $gitAvailable) {
            Append-Log "git.exe not found on PATH - skipping the git gc step." 'Warn'
        }
        elseif ($gitGcQueued -gt 0) {
            Append-Log "git gc --auto queued for $gitGcQueued repo(s) before their backup." 'Muted'
        }

        Start-BackupJobRun -QueueJobs $queueJobs -CopyJobCount $jobs.Count -RunProfile 'Project' -Destination $destFull
    }
    catch {
        Show-ThemedDialog -Message "Unexpected error: $($_.Exception.Message)" -Buttons OK -Kind Error | Out-Null
        # $script:Running must be reset BEFORE Set-StartButtonMode: it calls Update-ActionRow,
        # which reads $script:Running to decide whether the Project tab shows Full Backup/Sync or
        # the shared Start/Cancel button - reading it while still (wrongly) true here would leave
        # the action row stuck showing Cancel/Start Backup after this run actually aborted.
        $script:Running = $false
        Set-UiEnabled $true
        Set-StartButtonMode $false
    }
}

function Start-ProjectSyncRun {
    # Live Sync: you pick a direction, then that side becomes an exact mirror of the other -
    # new/changed files are copied AND anything missing from the source side is deleted from the
    # destination side. Reverse direction (Backup -> Project) can delete files in your live
    # project folders that were never backed up, so it gets its own stronger warning below.
    try {
        $dest = $txtDest.Text.Trim()
        if ([string]::IsNullOrWhiteSpace($dest)) {
            Show-ThemedDialog -Message 'Please choose a destination folder first.' -Buttons OK -Kind Warn | Out-Null
            return
        }
        if ($lstSources.Items.Count -eq 0) {
            Show-ThemedDialog -Message 'Add at least one source folder first.' -Buttons OK -Kind Warn | Out-Null
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

        $dirResult = Show-SyncDirectionDialog -ForwardLabel 'PROJECT  ->  BACKUP' -BackwardLabel 'BACKUP  ->  PROJECT'
        if ($dirResult -eq [System.Windows.Forms.DialogResult]::Yes) { $toBackup = $true }
        elseif ($dirResult -eq [System.Windows.Forms.DialogResult]::No) { $toBackup = $false }
        else { return }

        # Direction-tagged so a Backup -> Project run - the one that can delete files in your live
        # project folders - is never indistinguishable from a harmless Project -> Backup push, in
        # the live log headers or later in Backup History.
        $runProfile = if ($toBackup) { 'Sync ->' } else { 'Sync <-' }
        # A mirror run would otherwise purge .backstar-manifest.json as '*EXTRA' on the side that
        # doesn't have it (the live project folder never has one) - excluded from both directions
        # so a Full Backup's manifest survives a later forward Sync, and a backward Sync never
        # leaks backup-only metadata into the project folder.
        $syncExcludeFiles = @('.backstar-manifest.json')

        $jobs = New-Object System.Collections.Generic.List[object]
        $skipped = New-Object System.Collections.Generic.List[string]

        foreach ($item in $lstSources.Items) {
            $raw = (Get-RawSourcePath $item).TrimEnd('\')
            $name = Split-Path $raw -Leaf
            $destSub = Join-Path $destFull $name

            if ($toBackup) {
                if (-not (Test-Path -LiteralPath $raw -PathType Container)) {
                    $skipped.Add($raw)
                    continue
                }
                $srcFull = (Resolve-Path -LiteralPath $raw).Path.TrimEnd('\')
                if (-not (Test-DestNotInsideSource $destFull $srcFull)) {
                    Show-ThemedDialog -Message "The destination can't be the same as, inside, or contain a source folder:`n$srcFull" -Buttons OK -Kind Error | Out-Null
                    return
                }
                $jobs.Add((New-CopyJob -Name $name -Source $srcFull -Dest $destSub -Mirror $true -BackupProfile $runProfile `
                    -ExcludeDirNames $script:ExcludeDirs -ExcludeDirPaths $script:ExcludePaths -ExcludeFilePatterns $syncExcludeFiles))
            }
            else {
                # Backup -> Project: the backup copy must exist (nothing to sync from otherwise),
                # but the project folder is allowed to be missing - robocopy will recreate it,
                # which also makes this a valid way to restore an entirely deleted project folder.
                if (-not (Test-Path -LiteralPath $destSub -PathType Container)) {
                    $skipped.Add("$destSub (no backup copy to sync from yet)")
                    continue
                }
                $destSubFull = (Resolve-Path -LiteralPath $destSub).Path.TrimEnd('\')
                if (-not (Test-DestNotInsideSource $destSubFull $raw)) {
                    Show-ThemedDialog -Message "The destination can't be the same as, inside, or contain a source folder:`n$destSubFull" -Buttons OK -Kind Error | Out-Null
                    return
                }
                $jobs.Add((New-CopyJob -Name $name -Source $destSubFull -Dest $raw -Mirror $true -BackupProfile $runProfile `
                    -ExcludeDirNames $script:ExcludeDirs -ExcludeDirPaths $script:ExcludePaths -ExcludeFilePatterns $syncExcludeFiles))
            }
        }

        if ($jobs.Count -eq 0) {
            Show-ThemedDialog -Message 'Nothing to sync.' -Buttons OK -Kind Warn | Out-Null
            return
        }

        if ($toBackup) {
            $msg = "This will mirror $($jobs.Count) project folder(s) into the backup at:`n$destFull`n`nFiles or folders in the backup that no longer exist in the matching project folder will be PERMANENTLY DELETED so the backup matches your project exactly. Continue?"
            $confirmKind = 'Warn'
        }
        else {
            $msg = "This will mirror $($jobs.Count) backed-up folder(s) back into their original project location(s).`n`nFiles or folders in your PROJECT folders that no longer exist in the backup will be PERMANENTLY DELETED - including anything you haven't backed up yet. This cannot be undone. Continue?"
            $confirmKind = 'Error'
        }
        $confirm = Show-ThemedDialog -Message $msg -Title 'Confirm Live Sync' -Buttons YesNo -Kind $confirmKind
        if ($confirm -ne [System.Windows.Forms.DialogResult]::Yes) { return }

        $txtLog.Clear()
        foreach ($s in $skipped) { Append-Log "Skipping (not found on this machine): $s" 'Warn' }

        Start-BackupJobRun -QueueJobs $jobs -CopyJobCount $jobs.Count -RunProfile $runProfile -Destination $destFull
    }
    catch {
        Show-ThemedDialog -Message "Unexpected error: $($_.Exception.Message)" -Buttons OK -Kind Error | Out-Null
        # $script:Running must be reset BEFORE Set-StartButtonMode: it calls Update-ActionRow,
        # which reads $script:Running to decide whether the Project tab shows Full Backup/Sync or
        # the shared Start/Cancel button - reading it while still (wrongly) true here would leave
        # the action row stuck showing Cancel/Start Backup after this run actually aborted.
        $script:Running = $false
        Set-UiEnabled $true
        Set-StartButtonMode $false
    }
}
