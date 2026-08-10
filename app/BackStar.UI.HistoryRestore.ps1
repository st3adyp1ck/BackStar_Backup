# BackStar.UI.HistoryRestore.ps1 - Backup History viewer and (Phase 5) the Restore browser.
# Both are themed popup dialogs, disabled via their header buttons while a run is in progress so
# the live log/progress bar stays the single source of truth for what's currently happening.

function New-ThemedDialogShell {
    param([string]$Title, [int]$Width, [int]$Height, [int]$MinWidth = 0, [int]$MinHeight = 0)
    $dlg = New-Object System.Windows.Forms.Form
    $dlg.Text = $Title
    $dlg.FormBorderStyle = 'Sizable'
    $dlg.MinimizeBox = $false
    $dlg.MaximizeBox = $false
    $dlg.ShowInTaskbar = $false
    $dlg.StartPosition = 'CenterParent'
    $dlg.BackColor = $Theme.BgMain
    $dlg.Font = $Theme.FontRegular
    $dlg.ClientSize = New-Object System.Drawing.Size($Width, $Height)
    if ($MinWidth -gt 0) {
        $dlg.MinimumSize = New-Object System.Drawing.Size($MinWidth, $MinHeight)
    }
    $bar = New-Object System.Windows.Forms.Panel
    $bar.Size = New-Object System.Drawing.Size($Width, 4)
    $bar.Location = New-Object System.Drawing.Point(0, 0)
    $bar.BackColor = $Theme.AccentBlue
    $bar.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
    $dlg.Controls.Add($bar)
    $dlg.Add_Shown({ Set-DarkTitleBar $dlg })
    return $dlg
}

function Format-HistoryDuration([int]$seconds) {
    $mins = [Math]::Floor($seconds / 60)
    $secs = $seconds % 60
    return "{0:00}:{1:00}" -f $mins, $secs
}

function Show-HistoryDialog {
    $dlg = New-ThemedDialogShell -Title 'BackStar - Backup History' -Width 660 -Height 420 -MinWidth 500 -MinHeight 300

    $lv = New-Object System.Windows.Forms.ListView
    $lv.View = 'Details'
    $lv.FullRowSelect = $true
    $lv.GridLines = $false
    $lv.MultiSelect = $false
    $lv.BackColor = $Theme.BgPanel
    $lv.ForeColor = $Theme.TextPrimary
    $lv.BorderStyle = 'None'
    $lv.Font = $Theme.FontRegular
    $lv.Columns.Add('Date', 130) | Out-Null
    $lv.Columns.Add('Profile', 70) | Out-Null
    $lv.Columns.Add('Duration', 70) | Out-Null
    $lv.Columns.Add('Files', 60) | Out-Null
    $lv.Columns.Add('Destination', 210) | Out-Null
    $lv.Columns.Add('Result', 70) | Out-Null
    $pnlList = New-BorderPanel $lv $Theme.AccentBlue 2
    $pnlList.Location = New-Object System.Drawing.Point(12, 16)
    $pnlList.Size = New-Object System.Drawing.Size(636, 350)
    $pnlList.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right -bor [System.Windows.Forms.AnchorStyles]::Bottom
    $dlg.Controls.Add($pnlList)
    Set-DarkScrollbars $lv

    $history = @(Get-BackupHistory | Sort-Object { [datetime]$_.Timestamp } -Descending)
    foreach ($h in $history) {
        $dt = [datetime]$h.Timestamp
        $item = New-Object System.Windows.Forms.ListViewItem($dt.ToString('yyyy-MM-dd HH:mm'))
        $item.SubItems.Add($h.Profile) | Out-Null
        $item.SubItems.Add((Format-HistoryDuration $h.DurationSeconds)) | Out-Null
        $item.SubItems.Add("$($h.FilesCopied)") | Out-Null
        $item.SubItems.Add($h.Destination) | Out-Null
        $item.SubItems.Add($h.Result) | Out-Null
        if ($h.Result -eq 'Failed') { $item.ForeColor = $Theme.AccentRed }
        elseif ($h.Result -eq 'Cancelled') { $item.ForeColor = $Theme.AccentAmber }
        $item.Tag = $h
        $lv.Items.Add($item) | Out-Null
    }

    $lblEmpty = New-Object System.Windows.Forms.Label
    $lblEmpty.Text = 'No backups have run yet.'
    $lblEmpty.ForeColor = $Theme.TextMuted
    $lblEmpty.BackColor = $Theme.BgPanel
    $lblEmpty.AutoSize = $true
    $lblEmpty.Location = New-Object System.Drawing.Point(16, 16)
    $lblEmpty.Visible = ($history.Count -eq 0)
    $dlg.Controls.Add($lblEmpty)
    $lblEmpty.BringToFront()

    $btnClose = New-ThemedButton 'Close' $Theme.BgPanel $Theme.TextPrimary $Theme.TextMuted
    $btnClose.Size = New-Object System.Drawing.Size(90, 30)
    $btnClose.Location = New-Object System.Drawing.Point(558, 378)
    $btnClose.Anchor = [System.Windows.Forms.AnchorStyles]::Bottom -bor [System.Windows.Forms.AnchorStyles]::Right
    $btnClose.DialogResult = [System.Windows.Forms.DialogResult]::Cancel
    $dlg.Controls.Add($btnClose)
    $dlg.AcceptButton = $btnClose
    $dlg.CancelButton = $btnClose

    $btnRestoreFrom = New-ThemedButton 'Restore From Selected' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
    $btnRestoreFrom.Size = New-Object System.Drawing.Size(170, 30)
    $btnRestoreFrom.Location = New-Object System.Drawing.Point(12, 378)
    $btnRestoreFrom.Anchor = [System.Windows.Forms.AnchorStyles]::Bottom -bor [System.Windows.Forms.AnchorStyles]::Left
    $btnRestoreFrom.Enabled = $false
    $dlg.Controls.Add($btnRestoreFrom)

    $lv.Add_SelectedIndexChanged({
        $btnRestoreFrom.Enabled = ($lv.SelectedItems.Count -gt 0)
    })
    $btnRestoreFrom.Add_Click({
        if ($lv.SelectedItems.Count -eq 0) { return }
        $selectedDest = $lv.SelectedItems[0].Tag.Destination
        $dlg.Close()
        Show-RestoreDialog $selectedDest
    })

    $dlg.ShowDialog($script:form) | Out-Null
    $dlg.Dispose()
}

# ---------- restore browser ----------

function Add-RestoreTreeChildren([System.Windows.Forms.TreeNode]$node) {
    # Lazy population: a folder node starts with one empty dummy child so its expand glyph shows;
    # this replaces the dummy with the real children the first time it's needed. This IS the
    # loading/skeleton answer for this view - each expand only enumerates one directory level, so
    # there's never a moment where the tree looks broken while waiting on a big recursive scan.
    $path = $node.Tag
    if (-not (Test-Path -LiteralPath $path -PathType Container)) { return }
    $node.Nodes.Clear()
    foreach ($dir in (Get-ChildItem -LiteralPath $path -Directory -ErrorAction SilentlyContinue | Sort-Object Name)) {
        $child = New-Object System.Windows.Forms.TreeNode($dir.Name)
        $child.Tag = $dir.FullName
        $child.Nodes.Add((New-Object System.Windows.Forms.TreeNode(''))) | Out-Null
        $node.Nodes.Add($child) | Out-Null
    }
    foreach ($file in (Get-ChildItem -LiteralPath $path -File -ErrorAction SilentlyContinue | Sort-Object Name)) {
        if ($file.Name -eq '.backstar-manifest.json') { continue }
        $fnode = New-Object System.Windows.Forms.TreeNode($file.Name)
        $fnode.Tag = $file.FullName
        $node.Nodes.Add($fnode) | Out-Null
    }
}

function Set-RestoreNodeCheckedRecursive([System.Windows.Forms.TreeNode]$node, [bool]$checked) {
    $node.Checked = $checked
    if ($node.Nodes.Count -eq 1 -and $node.Nodes[0].Text -eq '' -and $node.Tag -and (Test-Path -LiteralPath $node.Tag -PathType Container)) {
        Add-RestoreTreeChildren $node
    }
    foreach ($child in $node.Nodes) { Set-RestoreNodeCheckedRecursive $child $checked }
}

function Get-RestoreTopLevelCheckedNodes($nodeCollection) {
    # A checked folder implies everything under it - only return the highest checked ancestor
    # in each branch so a selection isn't restored once as a folder and again per child.
    $result = New-Object System.Collections.Generic.List[object]
    foreach ($n in $nodeCollection) {
        if ($n.Checked) { $result.Add($n) }
        else { $result.AddRange((Get-RestoreTopLevelCheckedNodes $n.Nodes)) }
    }
    return $result
}

function Resolve-RestoreOriginalPath([System.Windows.Forms.TreeNode]$node) {
    $top = $node
    while ($top.Parent) { $top = $top.Parent }
    $manifestPath = Join-Path $top.Tag '.backstar-manifest.json'
    if (-not (Test-Path -LiteralPath $manifestPath)) { return $null }
    try {
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        $originalRoot = $manifest.OriginalSource
    }
    catch { return $null }
    if ([string]::IsNullOrWhiteSpace($originalRoot)) { return $null }
    $relative = $node.Tag.Substring($top.Tag.Length).TrimStart('\')
    if ([string]::IsNullOrEmpty($relative)) { return $originalRoot }
    return Join-Path $originalRoot $relative
}

function Show-RestoreDialog {
    param([string]$InitialDestination = $null)

    $dlg = New-ThemedDialogShell -Title 'BackStar - Restore' -Width 660 -Height 460 -MinWidth 500 -MinHeight 350

    $lblPath = New-Object System.Windows.Forms.Label
    $lblPath.Text = 'BACKUP LOCATION'
    $lblPath.Font = $Theme.FontBold
    $lblPath.ForeColor = $Theme.AccentBlue
    $lblPath.Location = New-Object System.Drawing.Point(12, 16)
    $lblPath.AutoSize = $true
    $dlg.Controls.Add($lblPath)

    $txtPath = New-Object System.Windows.Forms.TextBox
    $txtPath.BackColor = $Theme.BgPanel
    $txtPath.ForeColor = $Theme.TextPrimary
    $txtPath.BorderStyle = 'None'
    $txtPath.ReadOnly = $true
    $txtPath.Font = $Theme.FontRegular
    if ($InitialDestination) { $txtPath.Text = $InitialDestination }
    $pnlPath = New-BorderPanel $txtPath $Theme.AccentBlue 2
    $pnlPath.Location = New-Object System.Drawing.Point(12, 36)
    $pnlPath.Size = New-Object System.Drawing.Size(524, 27)
    $pnlPath.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
    $dlg.Controls.Add($pnlPath)

    $btnBrowsePath = New-ThemedButton 'Browse' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
    $btnBrowsePath.Location = New-Object System.Drawing.Point(546, 35)
    $btnBrowsePath.Size = New-Object System.Drawing.Size(102, 29)
    $btnBrowsePath.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
    $dlg.Controls.Add($btnBrowsePath)

    $tree = New-Object System.Windows.Forms.TreeView
    $tree.CheckBoxes = $true
    $tree.BackColor = $Theme.BgPanel
    $tree.ForeColor = $Theme.TextPrimary
    $tree.BorderStyle = 'None'
    $tree.Font = $Theme.FontRegular
    $pnlTree = New-BorderPanel $tree $Theme.AccentBlue 2
    $pnlTree.Location = New-Object System.Drawing.Point(12, 74)
    $pnlTree.Size = New-Object System.Drawing.Size(636, 292)
    $pnlTree.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right -bor [System.Windows.Forms.AnchorStyles]::Bottom
    $dlg.Controls.Add($pnlTree)
    Set-DarkScrollbars $tree

    $lblCount = New-Object System.Windows.Forms.Label
    $lblCount.Text = '0 item(s) selected'
    $lblCount.ForeColor = $Theme.TextMuted
    $lblCount.AutoSize = $true
    $lblCount.Location = New-Object System.Drawing.Point(12, 380)
    $lblCount.Anchor = [System.Windows.Forms.AnchorStyles]::Bottom -bor [System.Windows.Forms.AnchorStyles]::Left
    $dlg.Controls.Add($lblCount)

    $btnRestore = New-ThemedButton 'Restore Selected' $Theme.AccentRed $Theme.TextPrimary $Theme.AccentRed
    $btnRestore.Size = New-Object System.Drawing.Size(150, 32)
    $btnRestore.Location = New-Object System.Drawing.Point(498, 376)
    $btnRestore.Anchor = [System.Windows.Forms.AnchorStyles]::Bottom -bor [System.Windows.Forms.AnchorStyles]::Right
    $btnRestore.Enabled = $false
    $dlg.Controls.Add($btnRestore)

    $script:RestoreSuppressCheckEvents = $false
    function Update-RestoreSelectedCount {
        $count = (Get-RestoreTopLevelCheckedNodes $tree.Nodes).Count
        $lblCount.Text = "$count item(s) selected"
        $btnRestore.Enabled = ($count -gt 0)
    }

    function Set-RestoreRoot([string]$rootPath) {
        $tree.Nodes.Clear()
        if ([string]::IsNullOrWhiteSpace($rootPath) -or -not (Test-Path -LiteralPath $rootPath -PathType Container)) { return }
        foreach ($dir in (Get-ChildItem -LiteralPath $rootPath -Directory -ErrorAction SilentlyContinue | Sort-Object Name)) {
            $node = New-Object System.Windows.Forms.TreeNode($dir.Name)
            $node.Tag = $dir.FullName
            $node.Nodes.Add((New-Object System.Windows.Forms.TreeNode(''))) | Out-Null
            $tree.Nodes.Add($node) | Out-Null
        }
        Update-RestoreSelectedCount
    }

    $tree.Add_BeforeExpand({
        param($s, $e)
        if ($e.Node.Nodes.Count -eq 1 -and $e.Node.Nodes[0].Text -eq '') {
            Add-RestoreTreeChildren $e.Node
        }
    })
    $tree.Add_AfterCheck({
        param($s, $e)
        if ($script:RestoreSuppressCheckEvents) { return }
        $script:RestoreSuppressCheckEvents = $true
        Set-RestoreNodeCheckedRecursive $e.Node $e.Node.Checked
        $script:RestoreSuppressCheckEvents = $false
        Update-RestoreSelectedCount
    })

    $btnBrowsePath.Add_Click({
        $fbd = New-Object System.Windows.Forms.FolderBrowserDialog
        $fbd.Description = 'Select a backup destination folder to browse'
        $fbd.ShowNewFolderButton = $false
        if (-not [string]::IsNullOrWhiteSpace($txtPath.Text) -and (Test-Path -LiteralPath $txtPath.Text)) {
            $fbd.SelectedPath = $txtPath.Text
        }
        if ($fbd.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            $txtPath.Text = $fbd.SelectedPath.TrimEnd('\')
            Set-RestoreRoot $txtPath.Text
        }
    })

    $btnRestore.Add_Click({
        $selected = Get-RestoreTopLevelCheckedNodes $tree.Nodes
        if ($selected.Count -eq 0) { return }
        $msg = "Restore $($selected.Count) item(s) to their original location(s)?`n`nExisting files with the same name may be overwritten."
        $confirm = Show-ThemedDialog -Message $msg -Title 'Confirm Restore' -Buttons YesNo -Kind Warn
        if ($confirm -ne [System.Windows.Forms.DialogResult]::Yes) { return }
        $dlg.Close()
        Start-RestoreRun $selected
    })

    if ($InitialDestination) { Set-RestoreRoot $InitialDestination }

    $dlg.ShowDialog($script:form) | Out-Null
    $dlg.Dispose()
}

function Start-RestoreRun {
    param([System.Collections.Generic.List[object]]$SelectedNodes)

    $jobs = New-Object System.Collections.Generic.List[object]
    $unresolved = New-Object System.Collections.Generic.List[object]

    foreach ($node in $SelectedNodes) {
        $originalPath = Resolve-RestoreOriginalPath $node
        if (-not $originalPath) {
            $unresolved.Add($node)
            continue
        }
        if (Test-Path -LiteralPath $node.Tag -PathType Container) {
            $jobs.Add((New-CopyJob -Name "restore-$($node.Text)" -Source $node.Tag -Dest $originalPath -Mirror $false -BackupProfile 'Restore' -SkipManifest))
        }
        else {
            # A single file doesn't fit robocopy's directory-oriented model - just copy it directly.
            try {
                $destDir = Split-Path $originalPath -Parent
                if (-not (Test-Path -LiteralPath $destDir)) { New-Item -ItemType Directory -Path $destDir -Force | Out-Null }
                Copy-Item -LiteralPath $node.Tag -Destination $originalPath -Force
                Append-Log "Restored file: $originalPath" 'Success'
            }
            catch {
                Append-Log "FAILED to restore file '$($node.Tag)': $($_.Exception.Message)" 'Error'
            }
        }
    }

    foreach ($node in $unresolved) {
        # No manifest (e.g. a backup made before this feature existed, or the folder was moved) -
        # fall back to asking, same pattern as every other destination/source picker in the app.
        $fbd = New-Object System.Windows.Forms.FolderBrowserDialog
        $fbd.Description = "No record of where '$($node.Text)' came from - choose where to restore it to"
        if ($fbd.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
            if (Test-Path -LiteralPath $node.Tag -PathType Container) {
                $jobs.Add((New-CopyJob -Name "restore-$($node.Text)" -Source $node.Tag -Dest $fbd.SelectedPath -Mirror $false -BackupProfile 'Restore' -SkipManifest))
            }
            else {
                try {
                    Copy-Item -LiteralPath $node.Tag -Destination $fbd.SelectedPath -Force
                    Append-Log "Restored file: $($fbd.SelectedPath)" 'Success'
                }
                catch {
                    Append-Log "FAILED to restore file '$($node.Tag)': $($_.Exception.Message)" 'Error'
                }
            }
        }
    }

    if ($jobs.Count -eq 0) { return }

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
    $script:VerifyMismatches = 0
    $script:AnimPhase = 0
    $script:BarState = 'running'
    $barPanel.Invalidate()
    $script:RunStart = Get-Date
    $script:RunProfile = 'Restore'
    $script:RunDestination = $jobs[0].Dest

    Set-UiEnabled $false
    Set-StartButtonMode $true

    $script:Timer.Start()
    $script:AnimTimer.Start()
    Start-NextJob
}
