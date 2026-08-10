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
        }
        catch {
            Append-Log "Warning: could not read saved settings ($($_.Exception.Message))." 'Warn'
        }
    }
    if ([string]::IsNullOrWhiteSpace($txtDest.Text)) {
        $txtDest.Text = Join-Path $PSScriptRoot 'Backups'
    }
}

function Save-Config {
    $sources = @($lstSources.Items | ForEach-Object { Get-RawSourcePath $_ } | Where-Object { $_ })
    $cfg = [PSCustomObject]@{
        Destination = ConvertTo-StoredDestination $txtDest.Text.Trim()
        Sources     = $sources
        GitGc       = $chkGitGc.Checked
    }
    try {
        $cfg | ConvertTo-Json | Set-Content -LiteralPath $script:ConfigPath -Encoding UTF8
    }
    catch {
        Append-Log "Warning: could not save settings ($($_.Exception.Message))." 'Warn'
    }
}
