# BackStar.Theme.ps1 - color palette, fonts, native dark-chrome helpers, themed control factories.

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

# ---------- sync direction picker ----------

function Show-SyncDirectionDialog {
    # Three-way choice (forward / backward / cancel) reusing DialogResult the same way
    # Show-ThemedDialog's Yes/No buttons do: setting a button's DialogResult lets WinForms close
    # the dialog on click with no custom handler needed, sidestepping the closure-capture pitfall
    # noted above (event handler scriptblocks can't safely capture $dlg either).
    param(
        [string]$ForwardLabel,
        [string]$BackwardLabel
    )
    $width = 460
    $textAreaWidth = $width - 40
    $flags = [System.Windows.Forms.TextFormatFlags]::WordBreak -bor [System.Windows.Forms.TextFormatFlags]::Left
    $msg = 'Live Sync makes the destination side an exact mirror of the source side: new and changed files are copied, and anything no longer present on the source side is DELETED from the destination. Choose a direction.'
    $textSize = [System.Windows.Forms.TextRenderer]::MeasureText($msg, $Theme.FontRegular, (New-Object System.Drawing.Size($textAreaWidth, 0)), $flags)
    $lblHeight = [Math]::Max(36, $textSize.Height + 8)
    $btnH = 46
    $height = 24 + $lblHeight + 16 + $btnH + 12 + $btnH + 20 + 40

    $dlg = New-Object System.Windows.Forms.Form
    $dlg.Text = 'Choose Sync Direction'
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
    $bar.BackColor = $Theme.AccentAmber
    $dlg.Controls.Add($bar)

    $lbl = New-Object System.Windows.Forms.Label
    $lbl.Text = $msg
    $lbl.ForeColor = $Theme.TextPrimary
    $lbl.Location = New-Object System.Drawing.Point(20, 20)
    $lbl.Size = New-Object System.Drawing.Size($textAreaWidth, $lblHeight)
    $dlg.Controls.Add($lbl)

    $y = 20 + $lblHeight + 16

    $btnForward = New-ThemedButton $ForwardLabel $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
    $btnForward.Size = New-Object System.Drawing.Size($textAreaWidth, $btnH)
    $btnForward.Location = New-Object System.Drawing.Point(20, $y)
    $btnForward.DialogResult = [System.Windows.Forms.DialogResult]::Yes
    $dlg.Controls.Add($btnForward)
    $y += $btnH + 12

    $btnBackward = New-ThemedButton $BackwardLabel $Theme.BgPanel $Theme.AccentAmber $Theme.AccentAmber
    $btnBackward.Size = New-Object System.Drawing.Size($textAreaWidth, $btnH)
    $btnBackward.Location = New-Object System.Drawing.Point(20, $y)
    $btnBackward.DialogResult = [System.Windows.Forms.DialogResult]::No
    $dlg.Controls.Add($btnBackward)
    $y += $btnH + 20

    $btnCancel = New-ThemedButton 'Cancel' $Theme.BgMain $Theme.TextMuted $Theme.TextMuted
    $btnCancel.Size = New-Object System.Drawing.Size(90, 30)
    $btnCancel.Location = New-Object System.Drawing.Point(($width - 110), $y)
    $btnCancel.DialogResult = [System.Windows.Forms.DialogResult]::Cancel
    $dlg.Controls.Add($btnCancel)

    $dlg.CancelButton = $btnCancel

    $dlg.Add_Shown({ Set-DarkTitleBar $this })
    return $dlg.ShowDialog($script:form)
}
