#requires -version 5.0
# BackStar - portable USB backup tool: Project Backup (mirror) + System Backup (incremental).
# This is the entry point: it loads shared types/state, dot-sources the module files below
# (in the same script scope, so $script: state and functions are shared across all of them),
# builds the form shell + tab scaffold, then hands control to the tab modules and runs the form.
# Launched via BackStar.vbs -> BackStar.bat -> this file.

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
    [DllImport("shell32.dll")]
    public static extern int SetCurrentProcessExplicitAppUserModelID([MarshalAs(UnmanagedType.LPWStr)] string AppID);
}
"@

# Without this, Windows shows the powershell.exe host's own icon on the taskbar button instead of
# ours (the taskbar/jump-list identity defaults to grouping under "Windows PowerShell" unless the
# process claims its own distinct App User Model ID) - must be called before any window exists.
[BackStarNative]::SetCurrentProcessExplicitAppUserModelID('BackStar.BackupUtility') | Out-Null

$script:ExcludeDirs  = @('node_modules', 'dist', 'build', '.next', '__pycache__', '.venv', 'venv', 'target', '.gradle', '.dart_tool', '.expo')
# Source-relative dirs excluded by full path (matched per source at job start), because their
# folder name alone is too common to ban globally - e.g. Capacitor-bundled web build exports
# that 'npx cap sync' regenerates, both named 'public' like real source asset folders.
$script:ExcludePaths = @(
    'apps\mobile\ios\App\App\public',
    'apps\mobile\android\app\src\main\assets\public'
)
# System Backup junk/cache excludes - browser caches, thumbnail/temp files. Kept separate from
# $script:ExcludeDirs above since that list is tuned for source-code projects, not personal files.
$script:SystemExcludeDirs = @(
    'Cache', 'Code Cache', 'GPUCache', 'cache2', 'Service Worker', 'CacheStorage', 'blob_storage',
    'Crashpad', 'IndexedDB', '$RECYCLE.BIN', 'System Volume Information'
)
$script:SystemExcludeFiles = @('Thumbs.db', 'desktop.ini', '*.tmp')

# Single source of truth for both the in-app display below and the git tag created for each
# release (e.g. `git tag v$script:AppVersion`) - bump this alongside tagging a new release.
$script:AppVersion   = '1.0.0'
$script:ConfigPath   = Join-Path $PSScriptRoot 'BackStar.config.json'
$script:HistoryPath  = Join-Path $PSScriptRoot 'BackStar.history.json'
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
$script:VerifyMismatches = 0       # post-copy verification failures across the whole run
$script:ActiveTab    = 'Project'   # 'Project' | 'System' - which tab's config Start/Cancel acts on

# ---------- module loading ----------
# The '.' dot-source operator below (NOT '&') is required: it runs each file's code in the
# CALLER's scope, so $script: state and every function/variable a module defines lands in this
# script's own top-level scope, shared by every other module, as if this were still one file.
# That's also why the path-check below is a plain function but the actual dot-sourcing is NOT:
# dot-sourcing FROM INSIDE A FUNCTION would merge the module into that function's own throwaway
# scope instead of this script's scope, silently discarding everything the module defines the
# moment the function returns.

function Resolve-BackStarModulePath([string]$Name) {
    $modPath = Join-Path $PSScriptRoot $Name
    if (-not (Test-Path -LiteralPath $modPath)) {
        [System.Windows.Forms.MessageBox]::Show(
            "BackStar is missing a required file and cannot start:`n$modPath`n`nReinstall/redownload the BackStar folder so all files stay together.",
            'BackStar - Missing File', 'OK', 'Error') | Out-Null
        exit 1
    }
    return $modPath
}

. (Resolve-BackStarModulePath 'BackStar.Theme.ps1')
. (Resolve-BackStarModulePath 'BackStar.Config.ps1')
. (Resolve-BackStarModulePath 'BackStar.Engine.ps1')
. (Resolve-BackStarModulePath 'BackStar.Tray.ps1')
. (Resolve-BackStarModulePath 'BackStar.UI.HistoryRestore.ps1')

# ---------- form shell ----------

$form = New-Object System.Windows.Forms.Form
$form.Text = 'BackStar - Backup Utility'
$form.ClientSize = New-Object System.Drawing.Size(694, 704)
$form.MinimumSize = New-Object System.Drawing.Size(600, 604)
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
$lblSubtitle.Text = "DUAL BACKUP UTILITY - v$($script:AppVersion)"
$lblSubtitle.Font = $Theme.FontSubtitle
$lblSubtitle.ForeColor = $Theme.AccentRed
$lblSubtitle.BackColor = [System.Drawing.Color]::Transparent
$lblSubtitle.Location = New-Object System.Drawing.Point(86, 40)
$lblSubtitle.AutoSize = $true
$form.Controls.Add($lblSubtitle)

$btnRestore = New-ThemedButton 'Restore' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnRestore.Location = New-Object System.Drawing.Point(466, 18)
$btnRestore.Size = New-Object System.Drawing.Size(104, 26)
$btnRestore.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnRestore)

$btnHistory = New-ThemedButton 'History' $Theme.BgPanel $Theme.AccentBlue $Theme.AccentBlue
$btnHistory.Location = New-Object System.Drawing.Point(578, 18)
$btnHistory.Size = New-Object System.Drawing.Size(104, 26)
$btnHistory.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnHistory)

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

# --- tab strip: two toggle buttons + a sliding accent indicator underneath ---

$script:TabProjectX = 12
$script:TabSystemX  = 349
$script:TabIndicatorX = [double]$script:TabProjectX
$script:TabIndicatorTargetX = [double]$script:TabProjectX

$btnTabProject = New-ThemedButton 'PROJECT BACKUP' $Theme.BgPanel $Theme.AccentBlue $Theme.BgPanel
$btnTabProject.FlatAppearance.BorderSize = 0
$btnTabProject.Location = New-Object System.Drawing.Point($script:TabProjectX, 90)
$btnTabProject.Size = New-Object System.Drawing.Size(333, 30)
$btnTabProject.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnTabProject)

$btnTabSystem = New-ThemedButton 'SYSTEM BACKUP' $Theme.BgMain $Theme.TextMuted $Theme.BgMain
$btnTabSystem.FlatAppearance.BorderSize = 0
$btnTabSystem.Location = New-Object System.Drawing.Point($script:TabSystemX, 90)
$btnTabSystem.Size = New-Object System.Drawing.Size(333, 30)
$btnTabSystem.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnTabSystem)

$pnlTabIndicator = New-Object System.Windows.Forms.Panel
$pnlTabIndicator.Location = New-Object System.Drawing.Point($script:TabProjectX, 120)
$pnlTabIndicator.Size = New-Object System.Drawing.Size(333, 3)
$pnlTabIndicator.BackColor = $Theme.AccentBlue
$pnlTabIndicator.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left
$form.Controls.Add($pnlTabIndicator)

# --- tab content: two panels occupying the same rectangle; only one visible at a time ---

$pnlProjectTab = New-Object System.Windows.Forms.Panel
$pnlProjectTab.Location = New-Object System.Drawing.Point(12, 128)
$pnlProjectTab.Size = New-Object System.Drawing.Size(670, 260)
$pnlProjectTab.BackColor = $Theme.BgMain
$pnlProjectTab.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlProjectTab.Visible = $true
$form.Controls.Add($pnlProjectTab)

$pnlSystemTab = New-Object System.Windows.Forms.Panel
$pnlSystemTab.Location = New-Object System.Drawing.Point(12, 128)
$pnlSystemTab.Size = New-Object System.Drawing.Size(670, 260)
$pnlSystemTab.BackColor = $Theme.BgMain
$pnlSystemTab.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$pnlSystemTab.Visible = $false
$form.Controls.Add($pnlSystemTab)

# --- shared: Start/Cancel button, progress bar, status, log (used by whichever tab is running) ---

$btnActionFont = New-Object System.Drawing.Font('Consolas', 11, [System.Drawing.FontStyle]::Bold)

# Shared action row (y=396). $btnStart alone occupies it for the System tab and while any run
# (from either tab) is in progress - Cancel always needs the full width. On the Project tab, when
# idle, $btnFullBackup + $btnSync split it instead; see Update-ActionRow below, which is what
# actually decides which of these is visible and sizes the split pair (there's no such thing as
# "anchor to a sibling control" in WinForms, so their geometry can't be handled by Anchor alone).
$btnStart = New-ThemedButton 'Start Backup' $Theme.AccentRed $Theme.TextPrimary $Theme.AccentRed
$btnStart.Location = New-Object System.Drawing.Point(12, 396)
$btnStart.Size = New-Object System.Drawing.Size(670, 40)
$btnStart.Font = $btnActionFont
$btnStart.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($btnStart)

# Full Backup: additive only, never deletes anything at the destination - the safe default.
$btnFullBackup = New-ThemedButton 'Full Backup' $Theme.AccentBlue $Theme.BgMain $Theme.AccentBlue
$btnFullBackup.Location = New-Object System.Drawing.Point(12, 396)
$btnFullBackup.Size = New-Object System.Drawing.Size(468, 40)
$btnFullBackup.Font = $btnActionFont
$btnFullBackup.Visible = $false
$form.Controls.Add($btnFullBackup)

# Sync: mirrors project <-> backup in whichever direction you pick, including deletions.
$btnSync = New-ThemedButton 'Sync' $Theme.AccentRed $Theme.TextPrimary $Theme.AccentRed
$btnSync.Location = New-Object System.Drawing.Point(494, 396)
$btnSync.Size = New-Object System.Drawing.Size(188, 40)
$btnSync.Font = $btnActionFont
$btnSync.Visible = $false
$form.Controls.Add($btnSync)

$barPanel = New-Object System.Windows.Forms.Panel
$barPanel.Location = New-Object System.Drawing.Point(12, 444)
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
    elseif ($script:BarState -eq 'scanning') {
        $fillW = 0
    }
    elseif ($script:TotalJobs -gt 0) {
        $fillW = [int]($w * $script:DoneJobs / $script:TotalJobs)
    }
    if ($fillW -gt 0) {
        $fb = New-Object System.Drawing.SolidBrush($Theme.BarFill)
        $g.FillRectangle($fb, 0, 0, $fillW, $h)
        $fb.Dispose()
    }

    if (($script:BarState -eq 'running' -or $script:BarState -eq 'scanning') -and $fillW -lt $w) {
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
        'scanning'  { $text = 'SCANNING FOLDER SIZES...'; $textColor = $Theme.TextPrimary }
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
$lblStatus.Location = New-Object System.Drawing.Point(12, 474)
$lblStatus.Size = New-Object System.Drawing.Size(670, 18)
$lblStatus.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($lblStatus)

$lblLog = New-Object System.Windows.Forms.Label
$lblLog.Text = 'ACTIVITY LOG'
$lblLog.Font = $Theme.FontBold
$lblLog.ForeColor = $Theme.AccentBlue
$lblLog.Location = New-Object System.Drawing.Point(12, 498)
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
$pnlLog.Location = New-Object System.Drawing.Point(12, 518)
$pnlLog.Size = New-Object System.Drawing.Size(670, 174)
$pnlLog.Anchor = [System.Windows.Forms.AnchorStyles]::Top -bor [System.Windows.Forms.AnchorStyles]::Left -bor [System.Windows.Forms.AnchorStyles]::Right -bor [System.Windows.Forms.AnchorStyles]::Bottom
$form.Controls.Add($pnlLog)
Set-DarkScrollbars $txtLog

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

Initialize-TrayIcon

# ---------- tab modules ----------
# Each populates its own panel and defines the Start-*Run function(s) the action row below
# dispatches to. Project defines Start-ProjectFullBackupRun + Start-ProjectSyncRun (its own two
# split buttons); System defines Start-SystemBackupRun (dispatched from the shared $btnStart).

. (Resolve-BackStarModulePath 'BackStar.UI.ProjectTab.ps1')
. (Resolve-BackStarModulePath 'BackStar.UI.SystemTab.ps1')

# ---------- action row (Full Backup / Sync split vs. single Start Backup / Cancel) ----------
# Must come after both tab modules load: $btnFullBackup/$btnSync exist earlier (created above
# alongside $btnStart), but this also needs $script:ActiveTab, which BackStar.UI.ProjectTab.ps1/
# BackStar.UI.SystemTab.ps1 don't touch - it's only read here.

function Update-ActionRow {
    $showSplit = (-not $script:Running) -and ($script:ActiveTab -eq 'Project')
    $btnStart.Visible = -not $showSplit
    $btnFullBackup.Visible = $showSplit
    $btnSync.Visible = $showSplit

    if ($showSplit) {
        $gap = 12
        $syncW = 190
        $fullW = [Math]::Max(100, $btnStart.Width - $syncW - $gap)
        $btnFullBackup.Location = New-Object System.Drawing.Point($btnStart.Left, $btnStart.Top)
        $btnFullBackup.Size = New-Object System.Drawing.Size($fullW, $btnStart.Height)
        $btnSync.Location = New-Object System.Drawing.Point(($btnStart.Left + $fullW + $gap), $btnStart.Top)
        $btnSync.Size = New-Object System.Drawing.Size($syncW, $btnStart.Height)
    }
}
$form.Add_Resize({ Update-ActionRow })

# Also refreshes the action row (Set-StartButtonMode calls Update-ActionRow internally), now that
# every control it touches exists.
Set-StartButtonMode $false

# ---------- tab switching ----------

$script:TabAnimTimer = New-Object System.Windows.Forms.Timer
$script:TabAnimTimer.Interval = 15
$script:TabAnimTimer.Add_Tick({
    $diff = $script:TabIndicatorTargetX - $script:TabIndicatorX
    if ([Math]::Abs($diff) -lt 1) {
        $script:TabIndicatorX = $script:TabIndicatorTargetX
        $script:TabAnimTimer.Stop()
    }
    else {
        $script:TabIndicatorX += $diff * 0.25
    }
    $pnlTabIndicator.Location = New-Object System.Drawing.Point([int]$script:TabIndicatorX, 120)
})

function Set-ActiveTab([string]$tab) {
    $script:ActiveTab = $tab
    $isProject = ($tab -eq 'Project')
    $pnlProjectTab.Visible = $isProject
    $pnlSystemTab.Visible = -not $isProject

    $btnTabProject.BackColor = if ($isProject) { $Theme.BgPanel } else { $Theme.BgMain }
    $btnTabProject.ForeColor = if ($isProject) { $Theme.AccentBlue } else { $Theme.TextMuted }
    $btnTabSystem.BackColor = if ($isProject) { $Theme.BgMain } else { $Theme.BgPanel }
    $btnTabSystem.ForeColor = if ($isProject) { $Theme.TextMuted } else { $Theme.AccentRed }
    $pnlTabIndicator.BackColor = if ($isProject) { $Theme.AccentBlue } else { $Theme.AccentRed }

    $script:TabIndicatorTargetX = if ($isProject) { $script:TabProjectX } else { $script:TabSystemX }
    $script:TabAnimTimer.Start()

    if ($tab -eq 'System' -and -not $script:SystemTabSizeScanStarted) {
        $script:SystemTabSizeScanStarted = $true
        Start-FolderSizeScan
    }

    Update-ActionRow
}

$btnTabProject.Add_Click({ if (-not $script:Running) { Set-ActiveTab 'Project' } })
$btnTabSystem.Add_Click({ if (-not $script:Running) { Set-ActiveTab 'System' } })

$btnHistory.Add_Click({ if (-not $script:Running) { Show-HistoryDialog } })
$btnRestore.Add_Click({ if (-not $script:Running) { Show-RestoreDialog } })

# ---------- shared Start/Cancel dispatch ----------

$btnStart.Add_Click({
    if ($script:Running) {
        $script:Cancelled = $true
        Append-Log 'Cancelling...' 'Warn'
        $btnStart.Enabled = $false
        Stop-CurrentProcess
        return
    }
    # Idle + visible only happens on the System tab - the Project tab shows $btnFullBackup/
    # $btnSync instead (see Update-ActionRow).
    Start-SystemBackupRun
})

$btnFullBackup.Add_Click({ if (-not $script:Running) { Start-ProjectFullBackupRun } })
$btnSync.Add_Click({ if (-not $script:Running) { Start-ProjectSyncRun } })

# ---------- shared window events ----------

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
    if ($script:TrayIcon) {
        $script:TrayIcon.Visible = $false
        $script:TrayIcon.Dispose()
    }
})

$form.Add_Shown({
    $form.Activate()
    Set-DarkTitleBar $form
})

Load-Config

[System.Windows.Forms.Application]::Run($form)
