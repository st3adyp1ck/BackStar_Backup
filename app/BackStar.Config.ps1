# BackStar.Config.ps1 - path helpers and JSON config persistence.

# ---------- helpers ----------

function Get-RawSourcePath([string]$item) {
    if ($item.StartsWith($script:MissingTag)) { return $item.Substring($script:MissingTag.Length) }
    return $item
}

function Format-SourceItem([string]$path) {
    if (Test-Path -LiteralPath $path -PathType Container) { return $path }
    return "$script:MissingTag$path"
}

function ConvertTo-StoredDestination([string]$destFull) {
    if ([string]::IsNullOrWhiteSpace($destFull)) { return $destFull }
    $rootFull = $PSScriptRoot.TrimEnd('\')
    if ($destFull.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) { return '.' }
    if ($destFull.StartsWith("$rootFull\", [System.StringComparison]::OrdinalIgnoreCase)) {
        return '.\' + $destFull.Substring($rootFull.Length + 1)
    }
    return $destFull
}

function Resolve-StoredDestination([string]$stored) {
    if ([string]::IsNullOrWhiteSpace($stored)) { return $stored }
    if ($stored.StartsWith('.\') -or $stored.StartsWith('./')) {
        return (Join-Path $PSScriptRoot $stored.Substring(2))
    }
    if ($stored -eq '.') { return $PSScriptRoot }
    return $stored
}

function Get-BackupHistory {
    # Kept in its own sidecar file, deliberately separate from BackStar.config.json, so a large
    # or corrupt history file can never break settings loading (and vice versa).
    if (-not (Test-Path -LiteralPath $script:HistoryPath)) { return @() }
    try {
        $raw = Get-Content -LiteralPath $script:HistoryPath -Raw | ConvertFrom-Json
        return @($raw)
    }
    catch {
        return @()
    }
}

function Add-HistoryEntry {
    param(
        [string]$BackupProfile,
        [datetime]$StartedAt,
        [int]$DurationSeconds,
        [int]$FilesCopied,
        [int]$OkCount,
        [int]$FailCount,
        [string]$Destination,
        [string]$Result
    )
    $entries = New-Object System.Collections.Generic.List[object]
    $entries.AddRange(@(Get-BackupHistory))
    $entries.Add([PSCustomObject]@{
        Timestamp       = $StartedAt.ToString('o')
        Profile         = $BackupProfile
        DurationSeconds = $DurationSeconds
        FilesCopied     = $FilesCopied
        OkCount         = $OkCount
        FailCount       = $FailCount
        Destination     = $Destination
        Result          = $Result
    })
    # Cap growth: keep only the most recent 200 runs.
    $keep = if ($entries.Count -gt 200) { $entries.GetRange($entries.Count - 200, 200) } else { $entries }
    try {
        $keep | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $script:HistoryPath -Encoding UTF8
    }
    catch {
        Append-Log "Warning: could not save backup history ($($_.Exception.Message))." 'Warn'
    }
}

function Get-SystemPresetDefinitions {
    # Ordered {Key, Label, Path, Found} list backing the System Backup checklist. Path is resolved
    # fresh every launch via well-known folder APIs (never persisted) so config stays portable
    # across machines/accounts. A preset not present on this machine gets Found=$false; the UI
    # shows it disabled with a "(not installed)" suffix instead of erroring.
    $userProfile    = [Environment]::GetFolderPath('UserProfile')
    $localAppData   = [Environment]::GetFolderPath('LocalApplicationData')
    $roamingAppData = [Environment]::GetFolderPath('ApplicationData')
    $defs = @(
        [PSCustomObject]@{ Key = 'Desktop';   Label = 'Desktop';                        Path = [Environment]::GetFolderPath('Desktop') }
        [PSCustomObject]@{ Key = 'Documents'; Label = 'Documents';                      Path = [Environment]::GetFolderPath('MyDocuments') }
        [PSCustomObject]@{ Key = 'Pictures';  Label = 'Pictures';                       Path = [Environment]::GetFolderPath('MyPictures') }
        [PSCustomObject]@{ Key = 'Downloads'; Label = 'Downloads';                      Path = (Join-Path $userProfile 'Downloads') }
        [PSCustomObject]@{ Key = 'AppData';   Label = 'App Settings (AppData\Roaming)'; Path = $roamingAppData }
        [PSCustomObject]@{ Key = 'Chrome';    Label = 'Chrome (bookmarks & profile)';   Path = (Join-Path $localAppData 'Google\Chrome\User Data') }
        [PSCustomObject]@{ Key = 'Edge';      Label = 'Edge (bookmarks & profile)';     Path = (Join-Path $localAppData 'Microsoft\Edge\User Data') }
        [PSCustomObject]@{ Key = 'Firefox';   Label = 'Firefox (bookmarks & profile)';  Path = (Join-Path $roamingAppData 'Mozilla\Firefox\Profiles') }
    )
    foreach ($d in $defs) {
        $found = [bool]($d.Path -and (Test-Path -LiteralPath $d.Path -PathType Container))
        $d | Add-Member -NotePropertyName Found -NotePropertyValue $found
        $d | Add-Member -NotePropertyName SizeText -NotePropertyValue ''
        $d | Add-Member -NotePropertyName SizeBytes -NotePropertyValue ([long]-1)
    }
    return $defs
}

function Load-Config {
    if (Test-Path -LiteralPath $script:ConfigPath) {
        try {
            $cfg = Get-Content -LiteralPath $script:ConfigPath -Raw | ConvertFrom-Json
            if ($cfg.Sources) {
                foreach ($p in @($cfg.Sources)) {
                    if (-not [string]::IsNullOrWhiteSpace($p)) {
                        $lstSources.Items.Add((Format-SourceItem $p)) | Out-Null
                    }
                }
            }
            if ($cfg.Destination) {
                $txtDest.Text = Resolve-StoredDestination $cfg.Destination
            }
            if ($null -ne $cfg.GitGc) {
                $chkGitGc.Checked = [bool]$cfg.GitGc
            }

            # System Backup settings are additive/optional - old config files (or a first run)
            # simply won't have this key, and everything below just no-ops/falls back to defaults.
            $hadSystemPresetConfig = $false
            $sb = $cfg.SystemBackup
            if ($sb) {
                if ($sb.Destination) { $txtSystemDest.Text = Resolve-StoredDestination $sb.Destination }
                if ($sb.EnabledPresets) {
                    $hadSystemPresetConfig = $true
                    $enabled = @($sb.EnabledPresets)
                    foreach ($item in $lvCategories.Items) {
                        $item.Checked = ($enabled -contains $item.Tag.Key)
                    }
                }
                if ($sb.CustomSources) {
                    foreach ($p in @($sb.CustomSources)) {
                        if (-not [string]::IsNullOrWhiteSpace($p)) {
                            $lstSystemCustom.Items.Add((Format-SourceItem $p)) | Out-Null
                        }
                    }
                }
            }
            if (-not $hadSystemPresetConfig) {
                # First run, or an older config from before System Backup existed: default to
                # every preset actually found on this machine rather than starting empty.
                foreach ($item in $lvCategories.Items) {
                    if ($item.Tag.Found) { $item.Checked = $true }
                }
            }
        }
        catch {
            Append-Log "Warning: could not read saved settings ($($_.Exception.Message))." 'Warn'
        }
    }
    else {
        foreach ($item in $lvCategories.Items) {
            if ($item.Tag.Found) { $item.Checked = $true }
        }
    }
    if ([string]::IsNullOrWhiteSpace($txtDest.Text)) {
        $txtDest.Text = Join-Path $PSScriptRoot 'Backups'
    }
    if ([string]::IsNullOrWhiteSpace($txtSystemDest.Text)) {
        $txtSystemDest.Text = Join-Path $PSScriptRoot 'Backups\System'
    }
}

function Save-Config {
    $sources = @($lstSources.Items | ForEach-Object { Get-RawSourcePath $_ } | Where-Object { $_ })
    $enabledPresets = @($lvCategories.CheckedItems | ForEach-Object { $_.Tag.Key })
    $customSources = @($lstSystemCustom.Items | ForEach-Object { Get-RawSourcePath $_ } | Where-Object { $_ })
    $cfg = [PSCustomObject]@{
        Destination  = ConvertTo-StoredDestination $txtDest.Text.Trim()
        Sources      = $sources
        GitGc        = $chkGitGc.Checked
        SystemBackup = [PSCustomObject]@{
            Destination    = ConvertTo-StoredDestination $txtSystemDest.Text.Trim()
            EnabledPresets = $enabledPresets
            CustomSources  = $customSources
        }
    }
    try {
        $cfg | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $script:ConfigPath -Encoding UTF8
    }
    catch {
        Append-Log "Warning: could not save settings ($($_.Exception.Message))." 'Warn'
    }
}
