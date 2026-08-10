# BackStar.Tray.ps1 - system tray icon, minimize-to-tray, and completion notifications.
#
# Design choice: NotifyIcon.ShowBalloonTip is used directly rather than the WinRT toast APIs
# ([Windows.UI.Notifications.ToastNotificationManager]). Since Windows 10, a balloon tip shown
# via NotifyIcon is rendered by the shell as a real Action-Center-integrated notification - it is
# NOT the legacy Windows 7 bubble people picture. WinRT toasts need a valid AppUserModelID to
# call CreateToastNotifier() reliably, which an unpackaged plain powershell.exe process often
# doesn't have (a well-documented pain point), so they'd be a strictly worse choice here for
# zero added benefit.

function Initialize-TrayIcon {
    $script:TrayIcon = New-Object System.Windows.Forms.NotifyIcon
    $script:TrayIcon.Icon = $form.Icon
    $script:TrayIcon.Text = 'BackStar'
    $script:TrayIcon.Visible = $false

    $trayMenu = New-Object System.Windows.Forms.ContextMenuStrip
    $miOpen = $trayMenu.Items.Add('Open BackStar')
    $trayMenu.Items.Add('-') | Out-Null
    $miExit = $trayMenu.Items.Add('Exit')
    $script:TrayIcon.ContextMenuStrip = $trayMenu

    $miOpen.Add_Click({ Restore-FromTray })
    $miExit.Add_Click({
        $script:TrayIcon.Visible = $false
        $form.Close()
    })
    $script:TrayIcon.Add_DoubleClick({ Restore-FromTray })
    $script:TrayIcon.Add_BalloonTipClicked({ Restore-FromTray })

    $form.Add_Resize({
        if ($form.WindowState -eq [System.Windows.Forms.FormWindowState]::Minimized) {
            $form.ShowInTaskbar = $false
            $script:TrayIcon.Visible = $true
            $form.Hide()
        }
    })
}

function Restore-FromTray {
    $form.Show()
    $form.WindowState = [System.Windows.Forms.FormWindowState]::Normal
    $form.ShowInTaskbar = $true
    $script:TrayIcon.Visible = $false
    $form.Activate()
}

function Show-BackupNotification {
    param(
        [string]$Title,
        [string]$Message,
        [ValidateSet('Info', 'Warn', 'Error')] [string]$Kind = 'Info'
    )
    if (-not $script:TrayIcon -or $form.Visible) { return }
    $tipIcon = switch ($Kind) {
        'Error' { [System.Windows.Forms.ToolTipIcon]::Error }
        'Warn'  { [System.Windows.Forms.ToolTipIcon]::Warning }
        default { [System.Windows.Forms.ToolTipIcon]::Info }
    }
    try {
        $script:TrayIcon.BalloonTipTitle = $Title
        # Balloon tips render plain text; strip the newlines used in the themed-dialog version
        # of this same message so it doesn't show literal line breaks oddly.
        $script:TrayIcon.BalloonTipText = ($Message -replace "`n", '  ')
        $script:TrayIcon.BalloonTipIcon = $tipIcon
        $script:TrayIcon.ShowBalloonTip(5000)
    }
    catch { }
}
