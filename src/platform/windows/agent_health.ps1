# RigDesk's health sample: raw figures as one JSON line, never a verdict (Rig decides). Runs as SYSTEM in session 0
# every 10 minutes, read by rigdesk_agent::health::Figures (schema 1). A probe that fails is left out and named in errors.
$ErrorActionPreference = 'Stop'
$errors = New-Object System.Collections.Generic.List[string]
function Probe([string] $name, [scriptblock] $block) {
    try { & $block } catch { $errors.Add("${name}: $($_.Exception.Message)"); $null }
}
function UnixMs($when) {
    # Windows Update history keeps some entries with no date, as 0001-01-01: that is no date, not a negative time.
    if ($null -eq $when -or ([datetime] $when).Year -lt 1971) { return $null }
    ([DateTimeOffset] [datetime] $when).ToUnixTimeMilliseconds()
}
function Mb($bytes) { [int64] ([double] $bytes / 1MB) }

$os = Probe 'os' { Get-CimInstance Win32_OperatingSystem }
$ubr = Probe 'os build' { (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -Name UBR).UBR }

$cpu = Probe 'cpu' {
    $all = @(Get-CimInstance Win32_Processor)
    $loads = @($all | Where-Object { $null -ne $_.LoadPercentage } | ForEach-Object { [double] $_.LoadPercentage })
    [ordered] @{
        name = [string] $all[0].Name.Trim()
        load_pct = if ($loads.Count) { [double] ($loads | Measure-Object -Average).Average } else { $null }
    }
}

$volumes = @(Probe 'volumes' {
    Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | Where-Object { $_.Size } | ForEach-Object {
        [ordered] @{ name = [string] $_.DeviceID; label = [string] $_.VolumeName; total_mb = Mb $_.Size; free_mb = Mb $_.FreeSpace }
    }
})

$rebootWhy = @(Probe 'pending reboot' {
    if (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending') { 'component servicing' }
    if (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired') { 'windows update' }
    $renames = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager' -ErrorAction SilentlyContinue).PendingFileRenameOperations
    if ($renames) { 'file renames' }
})

# Windows Update's search is slow and may reach the network, so it runs at most hourly, kept beside this script.
$updates = Probe 'updates' {
    $kept = Join-Path $PSScriptRoot 'updates.json'
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    if (Test-Path $kept) {
        $last = Get-Content $kept -Raw | ConvertFrom-Json
        if ($last.checked_ms -and ($now - [int64] $last.checked_ms) -lt 3600000) { return $last }
    }
    $job = Start-Job {
        $searcher = (New-Object -ComObject Microsoft.Update.Session).CreateUpdateSearcher()
        $pending = $searcher.Search("IsInstalled=0 and IsHidden=0 and Type='Software'").Updates.Count
        $count = $searcher.GetTotalHistoryCount()
        $installed = if ($count -gt 0) {
            $searcher.QueryHistory(0, [Math]::Min($count, 100)) | Where-Object { $_.Operation -eq 1 -and $_.ResultCode -eq 2 } |
                Sort-Object Date -Descending | Select-Object -First 1 -ExpandProperty Date
        }
        @{ pending = $pending; installed = $installed }
    }
    $found = if (Wait-Job $job -Timeout 300) { Receive-Job $job -ErrorAction SilentlyContinue }
    $failed = if ($job.State -eq 'Failed') { $job.ChildJobs[0].JobStateInfo.Reason.Message } elseif ($job.State -ne 'Completed') { 'the search ran past 300 s' }
    Remove-Job $job -Force
    $result = [ordered] @{
        checked_ms = $now
        last_install_ms = if ($found -and $found.installed) { UnixMs ([datetime]::SpecifyKind([datetime] $found.installed, 'Utc')) } else { $null }
        pending = if ($found) { [int] $found.pending } else { $null }
        error = $failed
    }
    $result | ConvertTo-Json -Compress | Set-Content $kept
    $result
}

$defender = Probe 'defender' {
    $mp = Get-MpComputerStatus
    [ordered] @{ realtime_on = [bool] $mp.RealTimeProtectionEnabled; signature_age_days = [int] $mp.AntivirusSignatureAge }
}

$bitlocker = @(Probe 'bitlocker' {
    if (Get-Command Get-BitLockerVolume -ErrorAction SilentlyContinue) {
        Get-BitLockerVolume | ForEach-Object {
            [ordered] @{ volume = [string] $_.MountPoint; protection = [string] $_.ProtectionStatus; status = [string] $_.VolumeStatus }
        }
    }
})

$disks = @(Probe 'disks' {
    Get-PhysicalDisk | ForEach-Object {
        $counters = try { $_ | Get-StorageReliabilityCounter } catch { $null }
        [ordered] @{
            name = [string] $_.FriendlyName
            media = [string] $_.MediaType
            health = [string] $_.HealthStatus
            wear_pct = if ($counters -and $null -ne $counters.Wear) { [int] $counters.Wear } else { $null }
            temperature_c = if ($counters -and $counters.Temperature) { [int] $counters.Temperature } else { $null }
        }
    }
})

$stopped = @(Probe 'services' {
    Get-CimInstance Win32_Service -Filter "StartMode='Auto' AND State<>'Running'" | ForEach-Object {
        [ordered] @{ name = [string] $_.Name; display = [string] $_.DisplayName; delayed = [bool] $_.DelayedAutoStart; exit_code = [int64] $_.ExitCode }
    }
})

$systemErrors = Probe 'system log' {
    @(Get-WinEvent -FilterHashtable @{ LogName = 'System'; Level = 1, 2; StartTime = (Get-Date).AddHours(-24) } -ErrorAction SilentlyContinue).Count
}

$battery = Probe 'battery' {
    $b = @(Get-CimInstance Win32_Battery) | Select-Object -First 1
    if ($b) {
        $status = @{ 1 = 'discharging'; 2 = 'on AC'; 3 = 'full'; 4 = 'low'; 5 = 'critical'; 6 = 'charging' }[[int] $b.BatteryStatus]
        [ordered] @{ charge_pct = [int] $b.EstimatedChargeRemaining; status = if ($status) { $status } else { "status $($b.BatteryStatus)" } }
    }
}

# A probe that failed gives one null; the lists carry none.
$volumes = @($volumes | Where-Object { $null -ne $_ })
$rebootWhy = @($rebootWhy | Where-Object { $null -ne $_ })
$bitlocker = @($bitlocker | Where-Object { $null -ne $_ })
$disks = @($disks | Where-Object { $null -ne $_ })
$stopped = @($stopped | Where-Object { $null -ne $_ })

[ordered] @{
    schema = 1
    os = [ordered] @{
        caption = if ($os) { [string] $os.Caption } else { '' }
        version = if ($os) { [string] $os.Version } else { '' }
        build = if ($os) { if ($null -ne $ubr) { "$($os.BuildNumber).$ubr" } else { [string] $os.BuildNumber } } else { '' }
    }
    uptime_s = if ($os) { [int64] ((Get-Date) - $os.LastBootUpTime).TotalSeconds } else { 0 }
    cpu = if ($cpu) { $cpu } else { [ordered] @{ name = ''; load_pct = $null } }
    ram = if ($os) { [ordered] @{ total_mb = [int64] ($os.TotalVisibleMemorySize / 1024); free_mb = [int64] ($os.FreePhysicalMemory / 1024) } } else { [ordered] @{ total_mb = 0; free_mb = 0 } }
    volumes = $volumes
    pending_reboot = if ($errors | Where-Object { $_ -like 'pending reboot:*' }) { $null } else { [bool] $rebootWhy.Count }
    pending_reboot_why = $rebootWhy
    updates = $updates
    defender = $defender
    bitlocker = $bitlocker
    disks = $disks
    stopped_services = $stopped
    system_errors_24h = $systemErrors
    battery = $battery
    cpu_ms = [int64] (Get-Process -Id $PID).TotalProcessorTime.TotalMilliseconds
    errors = @($errors)
} | ConvertTo-Json -Compress -Depth 6
