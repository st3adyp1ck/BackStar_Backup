# BackStar.UI.SystemTab.ps1 - System Backup tab: preset categories + custom folders, incremental
# non-destructive copy (/E, never /MIR). Populates $pnlSystemTab (built by the entry script).
# Coordinates below are relative to that panel's own client area, not the form.

# --- backup categories ---

$lblCategories = New-Object System.Windows.Forms.Label
$lblCategories.Text = 'BACKUP CATEGORIES'
$lblCategories.Font = $Theme.FontBold
$lblCategories.ForeColor = $Theme.AccentBlue
$lblCategories.Location = New-Object System.Drawing.Point(0, 0)
$lblCategories.AutoSize = $true
$pnlSystemTab.Controls.Add($lblCategories)

$lvCategories = New-Object System.Windows.Forms.ListView
$lvCategories.View = 'Details'
$lvCategories.CheckBoxes = $true
$lvCategories.FullRowSelect = $true
$lvCategories.MultiSelect = $false
$lvCategories.HeaderStyle = 'None'
$lvCategories.GridLines = $false
$lvCategories.BackColor = $Theme.BgPanel
$lvCategories.ForeColor = $Theme.TextPrimary
$lvCategories.BorderStyle = 'None'
$lvCategories.Font = $Theme.FontRegular
$lvCategories.Columns.Add('Category', 388) | Out-Null
$lvCategories.Columns.Add('Size', 138) | Out-Null
$pnlCategories = New-BorderPanel $lvCategories $Theme.AccentBlue 2
$pnlCategories.Location = New-Object System.Drawing.Point(0, 20)
$pnlCategories.Size = New-Object System.Drawing.Size(550, 100)
$pnlCategories.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($pnlCategories)
Set-DarkScrollbars $lvCategories

$script:SystemPresets = Get-SystemPresetDefinitions
foreach ($p in $script:SystemPresets) {
    $item = New-Object System.Windows.Forms.ListViewItem($p.Label)
    $item.SubItems.Add('') | Out-Null
    $item.Tag = $p
    if (-not $p.Found) {
        $item.ForeColor = $Theme.TextMuted
        $item.SubItems[1].Text = 'not found on this PC'
    }
    $lvCategories.Items.Add($item) | Out-Null
}
# A preset that isn't present on this machine can't be checked - ListView items have no per-row
# Enabled property, so intercept the check instead and silently revert it.
$lvCategories.Add_ItemCheck({
    param($s, $e)
    $item = $lvCategories.Items[$e.Index]
    if ($item.Tag -and -not $item.Tag.Found -and $e.NewValue -eq [System.Windows.Forms.CheckState]::Checked) {
        $e.NewValue = [System.Windows.Forms.CheckState]::Unchecked
    }
})

$btnSelectAll = New-ThemedButton 'Select All' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnSelectAll.Location = New-Object System.Drawing.Point(562, 20)
$btnSelectAll.Size = New-Object System.Drawing.Size(108, 32)
$btnSelectAll.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($btnSelectAll)

$btnRescanSizes = New-ThemedButton 'Rescan Sizes' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnRescanSizes.Location = New-Object System.Drawing.Point(562, 56)
$btnRescanSizes.Size = New-Object System.Drawing.Size(108, 32)
$btnRescanSizes.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($btnRescanSizes)

# --- custom folders (same idea as Project Backup's source list) ---

$lblCustomFolders = New-Object System.Windows.Forms.Label
$lblCustomFolders.Text = 'CUSTOM FOLDERS'
$lblCustomFolders.Font = $Theme.FontBold
$lblCustomFolders.ForeColor = $Theme.AccentBlue
$lblCustomFolders.Location = New-Object System.Drawing.Point(0, 128)
$lblCustomFolders.AutoSize = $true
$pnlSystemTab.Controls.Add($lblCustomFolders)

$lstSystemCustom = New-Object System.Windows.Forms.ListBox
$lstSystemCustom.BackColor = $Theme.BgPanel
$lstSystemCustom.ForeColor = $Theme.TextPrimary
$lstSystemCustom.BorderStyle = 'None'
$lstSystemCustom.SelectionMode = 'MultiExtended'
$lstSystemCustom.HorizontalScrollbar = $true
$lstSystemCustom.IntegralHeight = $false
$lstSystemCustom.Font = $Theme.FontRegular
$pnlSystemCustom = New-BorderPanel $lstSystemCustom $Theme.AccentBlue 2
$pnlSystemCustom.Location = New-Object System.Drawing.Point(0, 148)
$pnlSystemCustom.Size = New-Object System.Drawing.Size(550, 55)
$pnlSystemCustom.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($pnlSystemCustom)
Set-DarkScrollbars $lstSystemCustom

$btnAddCustom = New-ThemedButton 'Add Folder' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnAddCustom.Location = New-Object System.Drawing.Point(562, 148)
$btnAddCustom.Size = New-Object System.Drawing.Size(108, 26)
$btnAddCustom.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($btnAddCustom)

$btnRemoveCustom = New-ThemedButton 'Remove' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnRemoveCustom.Location = New-Object System.Drawing.Point(562, 178)
$btnRemoveCustom.Size = New-Object System.Drawing.Size(108, 26)
$btnRemoveCustom.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($btnRemoveCustom)

# --- destination ---

$lblSystemDest = New-Object System.Windows.Forms.Label
$lblSystemDest.Text = 'DESTINATION'
$lblSystemDest.Font = $Theme.FontBold
$lblSystemDest.ForeColor = $Theme.AccentBlue
$lblSystemDest.Location = New-Object System.Drawing.Point(0, 211)
$lblSystemDest.AutoSize = $true
$pnlSystemTab.Controls.Add($lblSystemDest)

$txtSystemDest = New-Object System.Windows.Forms.TextBox
$txtSystemDest.BackColor = $Theme.BgPanel
$txtSystemDest.ForeColor = $Theme.TextPrimary
$txtSystemDest.BorderStyle = 'None'
$txtSystemDest.ReadOnly = $true
$txtSystemDest.Font = $Theme.FontRegular
$pnlSystemDest = New-BorderPanel $txtSystemDest $Theme.AccentBlue 2
$pnlSystemDest.Location = New-Object System.Drawing.Point(0, 231)
$pnlSystemDest.Size = New-Object System.Drawing.Size(550, 27)
$pnlSystemDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($pnlSystemDest)

$btnBrowseSystemDest = New-ThemedButton 'Browse' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnBrowseSystemDest.Location = New-Object System.Drawing.Point(562, 230)
$btnBrowseSystemDest.Size = New-Object System.Drawing.Size(108, 29)
$btnBrowseSystemDest.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Controls.Add($btnBrowseSystemDest)

# ---------- async folder-size scan ----------
# Reuses the app's "hidden background work + poll on a timer" idiom, but via a PowerShell
# background job instead of a redirected-log child process, since there's no line-oriented output
# to tail here - just a small result set once the whole scan finishes. Never blocks Start Backup.

$script:SizeScanJob = $null
$script:SystemTabSizeScanStarted = $false

$script:SizeScanTimer = New-Object System.Windows.Forms.Timer
$script:SizeScanTimer.Interval = 500
$script:SizeScanTimer.Add_Tick({
    if (-not $script:SizeScanJob) { $script:SizeScanTimer.Stop(); return }
    if ($script:SizeScanJob.State -notin @('Completed', 'Failed', 'Stopped')) { return }

    $results = @(Receive-Job -Job $script:SizeScanJob -ErrorAction SilentlyContinue)
    Remove-Job -Job $script:SizeScanJob -Force -ErrorAction SilentlyContinue
    $script:SizeScanJob = $null
    $script:SizeScanTimer.Stop()
    if ($script:BarState -eq 'scanning') {
        $script:BarState = 'idle'
        if (-not $script:Running) { $script:AnimTimer.Stop() }
        $barPanel.Invalidate()
    }
    foreach ($r in $results) {
        foreach ($item in $lvCategories.Items) {
            if ($item.Tag.Key -eq $r.Key) {
                $item.Tag.SizeBytes = $r.Bytes
                $item.Tag.SizeText = $r.Text
                $item.SubItems[1].Text = $r.Text
            }
        }
    }
    if ($results.Count -gt 0) { Append-Log 'Folder size scan complete.' 'Muted' }
})

function Start-FolderSizeScan {
    if ($script:SizeScanJob -or $script:Running) { return }
    $targets = @($lvCategories.Items | Where-Object { $_.Tag.Found } | ForEach-Object { [PSCustomObject]@{ Key = $_.Tag.Key; Path = $_.Tag.Path } })
    if ($targets.Count -eq 0) { return }
    foreach ($item in $lvCategories.Items) {
        if ($item.Tag.Found) { $item.SubItems[1].Text = 'scanning...' }
    }

    $script:BarState = 'scanning'
    $script:AnimPhase = 0
    $barPanel.Invalidate()
    $script:AnimTimer.Start()

    $script:SizeScanJob = Start-Job -ScriptBlock {
        param($targets)
        $results = @()
        foreach ($t in $targets) {
            $bytes = 0L
            try {
                $sum = (Get-ChildItem -LiteralPath $t.Path -Recurse -File -Force -ErrorAction SilentlyContinue |
                    Measure-Object -Property Length -Sum).Sum
                if ($sum) { $bytes = [long]$sum }
            }
            catch { $bytes = 0L }
            $text = if ($bytes -ge 1GB) { '{0:N1} GB' -f ($bytes / 1GB) }
                    elseif ($bytes -ge 1MB) { '{0:N0} MB' -f ($bytes / 1MB) }
                    elseif ($bytes -ge 1KB) { '{0:N0} KB' -f ($bytes / 1KB) }
                    else { "$bytes B" }
            $results += [PSCustomObject]@{ Key = $t.Key; Bytes = $bytes; Text = $text }
        }
        $results
    } -ArgumentList (, $targets)

    $script:SizeScanTimer.Start()
}

# ---------- events ----------

$btnSelectAll.Add_Click({
    $foundItems = @($lvCategories.Items | Where-Object { $_.Tag.Found })
    $allChecked = ($foundItems.Count -gt 0) -and (@($foundItems | Where-Object { -not $_.Checked }).Count -eq 0)
    foreach ($item in $foundItems) { $item.Checked = -not $allChecked }
})

$btnRescanSizes.Add_Click({ Start-FolderSizeScan })

$btnAddCustom.Add_Click({
    try {
        $fbd = New-Object System.Windows.Forms.FolderBrowserDialog
        $fbd.Description = 'Select a folder to back up'
        $fbd.ShowNewFolderButton = $false
        if ($fbd.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            if ($fbd.SelectedPath -eq [System.IO.Path]::GetPathRoot($fbd.SelectedPath)) {
                Show-ThemedDialog -Message 'Please pick a folder, not a whole drive root.' -Buttons OK -Kind Warn | Out-Null
                return
            }
            $path = $fbd.SelectedPath.TrimEnd('\')
            $existingPaths = @($lstSystemCustom.Items | ForEach-Object { Get-RawSourcePath $_ })

            if ($existingPaths -contains $path) {
                Show-ThemedDialog -Message 'That folder is already in the list.' -Buttons OK -Kind Info | Out-Null
                return
            }

            $name = Split-Path $path -Leaf
            $existingNames = @($existingPaths | ForEach-Object { Split-Path $_ -Leaf })
            $presetNames = @($lvCategories.Items | ForEach-Object { $_.Tag.Key })
            if (($existingNames -contains $name) -or ($presetNames -contains $name)) {
                Show-ThemedDialog -Message "Another selected item already uses the name '$name'. Each item needs a unique folder name, since that name becomes its backup subfolder." -Buttons OK -Kind Warn | Out-Null
                return
            }

            $lstSystemCustom.Items.Add((Format-SourceItem $path)) | Out-Null
        }
    }
    catch {
        Show-ThemedDialog -Message "Error adding folder: $($_.Exception.Message)" -Buttons OK -Kind Error | Out-Null
    }
})

$btnRemoveCustom.Add_Click({
    foreach ($item in @($lstSystemCustom.SelectedItems)) {
        $lstSystemCustom.Items.Remove($item)
    }
})

$btnBrowseSystemDest.Add_Click({
    try {
        $fbd = New-Object System.Windows.Forms.FolderBrowserDialog
        $fbd.Description = 'Select the System Backup destination folder'
        $fbd.ShowNewFolderButton = $true
        if (-not [string]::IsNullOrWhiteSpace($txtSystemDest.Text) -and (Test-Path -LiteralPath $txtSystemDest.Text)) {
            $fbd.SelectedPath = $txtSystemDest.Text
        }
        if ($fbd.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            $txtSystemDest.Text = $fbd.SelectedPath.TrimEnd('\')
        }
    }
    catch {
        Show-ThemedDialog -Message "Error selecting destination: $($_.Exception.Message)" -Buttons OK -Kind Error | Out-Null
    }
})

function Start-SystemBackupRun {
    try {
        $dest = $txtSystemDest.Text.Trim()
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
        $usedNames = New-Object System.Collections.Generic.HashSet[string]([System.StringComparer]::OrdinalIgnoreCase)

        function Test-DestNotInsideSource([string]$destFull, [string]$srcFull) {
            return -not ($destFull.Equals($srcFull, [System.StringComparison]::OrdinalIgnoreCase) -or
                $destFull.StartsWith("$srcFull\", [System.StringComparison]::OrdinalIgnoreCase) -or
                $srcFull.StartsWith("$destFull\", [System.StringComparison]::OrdinalIgnoreCase))
        }

        foreach ($item in $lvCategories.Items) {
            if (-not $item.Checked -or -not $item.Tag.Found) { continue }
            $srcFull = $item.Tag.Path.TrimEnd('\')
            $name = $item.Tag.Key
            if (-not (Test-DestNotInsideSource $destFull $srcFull)) {
                Show-ThemedDialog -Message "The destination can't be the same as, inside, or contain a source folder:`n$srcFull" -Buttons OK -Kind Error | Out-Null
                return
            }
            $usedNames.Add($name) | Out-Null
            $jobs.Add((New-CopyJob -Name $name -Source $srcFull -Dest (Join-Path $destFull $name) -Mirror $false -BackupProfile 'System' `
                -ExcludeDirNames $script:SystemExcludeDirs -ExcludeFilePatterns $script:SystemExcludeFiles))
        }

        $skipped = New-Object System.Collections.Generic.List[string]
        foreach ($item in $lstSystemCustom.Items) {
            $raw = Get-RawSourcePath $item
            if (-not (Test-Path -LiteralPath $raw -PathType Container)) {
                $skipped.Add($raw)
                continue
            }
            $srcFull = (Resolve-Path -LiteralPath $raw).Path.TrimEnd('\')
            $name = Split-Path $srcFull -Leaf
            if (-not $usedNames.Add($name)) {
                Show-ThemedDialog -Message "'$name' collides with another item's backup folder name (a category or another custom folder). Rename or remove one of them." -Buttons OK -Kind Warn | Out-Null
                return
            }
            if (-not (Test-DestNotInsideSource $destFull $srcFull)) {
                Show-ThemedDialog -Message "The destination can't be the same as, inside, or contain a source folder:`n$srcFull" -Buttons OK -Kind Error | Out-Null
                return
            }
            $jobs.Add((New-CopyJob -Name $name -Source $srcFull -Dest (Join-Path $destFull $name) -Mirror $false -BackupProfile 'System' `
                -ExcludeDirNames $script:SystemExcludeDirs -ExcludeFilePatterns $script:SystemExcludeFiles))
        }

        if ($jobs.Count -eq 0) {
            Show-ThemedDialog -Message 'Nothing selected to back up. Check a category above or add a custom folder first.' -Buttons OK -Kind Warn | Out-Null
            return
        }

        $msg = "This will copy $($jobs.Count) item(s) into:`n$destFull`n`nOnly new or changed files are copied. Nothing already at the destination is ever deleted. Continue?"
        $confirm = Show-ThemedDialog -Message $msg -Title 'Confirm System Backup' -Buttons YesNo -Kind Info
        if ($confirm -ne [System.Windows.Forms.DialogResult]::Yes) { return }

        Save-Config

        $script:Running = $true
        $script:Cancelled = $false
        $script:ResultsSummary = New-Object System.Collections.Generic.List[string]
        $script:JobQueue = New-Object System.Collections.Generic.Queue[object]
        foreach ($j in $jobs) { $script:JobQueue.Enqueue($j) }
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

        Set-UiEnabled $false
        Set-StartButtonMode $true
        $script:RunStart = Get-Date
        $script:RunProfile = 'System'
        $script:RunDestination = $destFull

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
}
