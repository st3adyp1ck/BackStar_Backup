#requires -version 5.0
# BackStar - portable USB backup tool: Project Backup (mirror) + System Backup (incremental).
# This is the entry point: it loads shared types/state, dot-sources the module files below
# (in the same script scope, so $script: state and functions are shared across all of them),
# then builds/runs the form. Launched via BackStar.vbs -> BackStar.bat -> this file.

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

# ---------- module loading ----------
# Dot-sourcing (not '&') is required: it runs each file's code in THIS script's scope, so
# $script: state and every function defined below are shared across all modules as if this
# were still one file. Order matters - later modules use functions/controls the earlier ones define.

$script:Modules = @(
    'BackStar.Theme.ps1'
    'BackStar.Config.ps1'
    'BackStar.Engine.ps1'
    'BackStar.UI.ProjectTab.ps1'
)
foreach ($m in $script:Modules) {
    $modPath = Join-Path $PSScriptRoot $m
    if (-not (Test-Path -LiteralPath $modPath)) {
        [System.Windows.Forms.MessageBox]::Show(
            "BackStar is missing a required file and cannot start:`n$modPath`n`nReinstall/redownload the BackStar folder so all files stay together.",
            'BackStar - Missing File', 'OK', 'Error') | Out-Null
        exit 1
    }
    . $modPath
}

Load-Config

[System.Windows.Forms.Application]::Run($form)
