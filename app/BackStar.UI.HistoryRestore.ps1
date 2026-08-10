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

    $dlg.ShowDialog($script:form) | Out-Null
    $dlg.Dispose()
}
