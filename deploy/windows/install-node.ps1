#Requires -Version 5.1
<#
.SYNOPSIS
    Installs (or removes) `fleet node` on a Windows PC as a Scheduled Task that
    starts at logon and runs as the current user.

.DESCRIPTION
    Why a Scheduled Task at logon, and not a Windows service:

    A node starts Claude Code, and Claude Code's login lives in the user's
    profile (%USERPROFILE%\.claude). So do the user's git credentials (Git
    Credential Manager keeps them in Windows Credential Manager, protected by
    DPAPI with the user's logon secret), SSH keys, PATH, toolchains and
    repositories. A service runs as LocalSystem or a service account with none
    of that. A service configured to run as the user needs the user's password
    stored in the SCM, and still does not get an interactive logon, so DPAPI-
    protected credentials can fail. An "S4U" scheduled task ("run whether user
    is logged on or not") has the same DPAPI problem.

    An Interactive scheduled task at logon runs with the user's real token, so
    agents behave exactly as when the user runs `claude` in a terminal. The
    cost: the node runs only while the user is logged on. Use auto-logon or
    keep the session logged on (locking the screen is fine) on PCs that should
    always be available.

    The task runs a small launcher that restarts `fleet node` whenever it exits
    (the equivalent of systemd Restart=always). This covers crashes and self-
    updates (the node exits so the new binary starts). Task Scheduler's own
    restart-on-failure only reacts to some failures, so it is not relied on.

.PARAMETER FleetExe
    Path to fleet.exe from the release (fleet-<version>-x86_64-pc-windows-msvc.exe
    renamed, or as is). Default: .\fleet.exe next to this script.

.PARAMETER TrayExe
    Path to fleet-tray.exe, the tray app for mapping projects to folders
    (fleet-tray-<version>-x86_64-pc-windows-msvc.exe from the release, renamed or
    as is). Optional. Default: the newest of fleet-tray.exe and
    fleet-tray-*-x86_64-pc-windows-msvc.exe next to -FleetExe, if there is one.
    Installed next to fleet.exe, where `fleet node` looks for it and starts it.

.PARAMETER InstallDir
    Where fleet.exe and the launcher are copied. Added to the user's PATH so
    agents can run `fleet secret get` and `fleet mcp`.

.PARAMETER Relay
    Value for FLEET_RELAY (user environment): the orchestrator's relay URL,
    e.g. https://relay.example.com. Optional.

.PARAMETER Ticket
    One-time ticket from `fleet invite` on the VPS. When given, runs
    `fleet join <ticket>` before registering the task.

.PARAMETER Uninstall
    Stops and removes the task, the launcher, the binary and the PATH entry.
    Keeps the node's data directory (its iroh key) unless -RemoveData is also given.

.PARAMETER RemoveData
    With -Uninstall: also deletes the node's data directory (FLEET_DATA_DIR,
    default %LOCALAPPDATA%\fleet\data). The node then needs a new invite.

.EXAMPLE
    # From an ordinary (non-elevated) PowerShell, as the user logged into Claude Code:
    powershell -ExecutionPolicy Bypass -File .\install-node.ps1 -FleetExe .\fleet.exe `
        -TrayExe .\fleet-tray.exe -Relay https://relay.example.com -Ticket <ticket from `fleet invite`>

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\install-node.ps1 -Uninstall
#>
[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
    [Parameter(ParameterSetName = 'Install')]
    [string]$FleetExe = (Join-Path $PSScriptRoot 'fleet.exe'),

    [Parameter(ParameterSetName = 'Install')]
    [string]$TrayExe,

    [Parameter(ParameterSetName = 'Install')]
    [Parameter(ParameterSetName = 'Uninstall')]
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\fleet'),

    [Parameter(ParameterSetName = 'Install')]
    [string]$Relay,

    [Parameter(ParameterSetName = 'Install')]
    [string]$Ticket,

    [Parameter(ParameterSetName = 'Uninstall', Mandatory = $true)]
    [switch]$Uninstall,

    [Parameter(ParameterSetName = 'Uninstall')]
    [switch]$RemoveData
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$TaskName = 'fleet node'
# Root folder: creating a task folder can need elevation; tasks in the root do not.
$TaskPath = '\'
$DataDir = if ($env:FLEET_DATA_DIR) { $env:FLEET_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'fleet\data' }
$LogDir = Join-Path $env:LOCALAPPDATA 'fleet\logs'
$ExePath = Join-Path $InstallDir 'fleet.exe'
$TrayPath = Join-Path $InstallDir 'fleet-tray.exe'
$Launcher = Join-Path $InstallDir 'fleet-node-launcher.ps1'
$UserId = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name

function Remove-FromUserPath([string]$dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { return }
    $parts = @($current.Split(';') | Where-Object { $_ -and ($_.TrimEnd('\') -ne $dir.TrimEnd('\')) })
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

function Add-ToUserPath([string]$dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    # @() keeps a one-entry PATH an array, so + appends instead of concatenating strings.
    $parts = @()
    if ($current) { $parts = @($current.Split(';') | Where-Object { $_ }) }
    if ($parts | Where-Object { $_.TrimEnd('\') -eq $dir.TrimEnd('\') }) { return }
    [Environment]::SetEnvironmentVariable('Path', ((@($parts) + $dir) -join ';'), 'User')
}

function Stop-FleetNode {
    $task = Get-ScheduledTask -TaskName $TaskName -TaskPath $TaskPath -ErrorAction SilentlyContinue
    if ($task) {
        # Ends the launcher; the node is its child and is stopped below.
        Stop-ScheduledTask -TaskName $TaskName -TaskPath $TaskPath -ErrorAction SilentlyContinue
    }
    # Stop node processes started from our install dir (not other fleet.exe copies). The tray
    # exits by itself once the node is gone, but only after a poll; stop it now too so
    # fleet-tray.exe can be replaced or removed.
    $procs = @(Get-Process -Name 'fleet', 'fleet-tray' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and (($_.Path -ieq $ExePath) -or ($_.Path -ieq $TrayPath)) })
    # The launcher loop itself, in case it outlived its console host: it would start the old
    # node again five seconds later.
    $launchers = @(Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine.Contains($Launcher) })
    $launchers | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    $procs | Stop-Process -Force -ErrorAction SilentlyContinue
    # Stop-Process returns before the processes are gone, and their exes stay locked until then.
    $procs | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
}

if ($Uninstall) {
    Write-Host "Removing scheduled task '$TaskPath$TaskName'..."
    Stop-FleetNode
    Unregister-ScheduledTask -TaskName $TaskName -TaskPath $TaskPath -Confirm:$false -ErrorAction SilentlyContinue

    Start-Sleep -Seconds 2  # let the processes release the files
    if (Test-Path $InstallDir) { Remove-Item -Recurse -Force $InstallDir }
    Remove-FromUserPath $InstallDir

    if ($RemoveData) {
        if (Test-Path $DataDir) { Remove-Item -Recurse -Force $DataDir }
        $logs = Join-Path $env:LOCALAPPDATA 'fleet'
        if (Test-Path $logs) { Remove-Item -Recurse -Force $logs }
        Write-Host "Removed the node's data ($DataDir). This node needs a new invite to rejoin."
    } else {
        Write-Host "Kept the node's data and iroh key in $DataDir (use -RemoveData to delete)."
    }
    Write-Host 'fleet node uninstalled.'
    return
}

# ---- install ----------------------------------------------------------------

if (-not (Test-Path $FleetExe)) {
    throw "fleet.exe not found at '$FleetExe'. Pass -FleetExe <path to the release binary>."
}
if ($TrayExe) {
    if (-not (Test-Path $TrayExe)) { throw "fleet-tray.exe not found at '$TrayExe'." }
} else {
    # Optional: without it the node simply runs without a tray icon. Next to -FleetExe, take
    # fleet-tray.exe or a release asset (fleet-tray-<version>-x86_64-pc-windows-msvc.exe), the
    # newest if there are several.
    $candidate = Get-ChildItem -LiteralPath (Split-Path -Parent (Resolve-Path $FleetExe)) -File |
        Where-Object { $_.Name -ieq 'fleet-tray.exe' -or $_.Name -like 'fleet-tray-*-x86_64-pc-windows-msvc.exe' } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if ($candidate) { $TrayExe = $candidate.FullName }
}
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).
    IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($isAdmin) {
    Write-Warning ('Running elevated. The task is registered for the current user ({0}); make sure ' +
        'that is the account logged into Claude Code.' -f $UserId)
}

if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Write-Warning ("'claude' is not on PATH for $UserId. Install Claude Code and log in " +
        '(run `claude`, then /login) before agents can start on this node.')
}

# Upgrades: stop the running node so fleet.exe can be replaced.
Stop-FleetNode

New-Item -ItemType Directory -Force -Path $InstallDir, $DataDir, $LogDir | Out-Null
# Re-running the installer with the installed files as the source (to re-register the
# task, say) must not fail on copying a file onto itself. A process that was just stopped
# can hold its exe for a few seconds more (the tray does), so a locked target is retried
# for up to 30 s before giving up.
function Copy-IfOther($From, $To) {
    $src = (Resolve-Path -LiteralPath $From).ProviderPath
    if ((Test-Path -LiteralPath $To) -and ($src -ieq (Resolve-Path -LiteralPath $To).ProviderPath)) { return }
    for ($i = 1; ; $i++) {
        try { Copy-Item -Force -LiteralPath $src $To -ErrorAction Stop; return }
        catch [System.IO.IOException] {
            if ($i -ge 30) { throw }
            Start-Sleep -Seconds 1
        }
    }
}
Copy-IfOther $FleetExe $ExePath
# What a self-update leaves beside the binary belongs to the binary that was
# there before: an older fleet.old.exe the launcher could restore over the one
# just installed, a download, a rejected release. A fresh install starts clean.
Get-ChildItem -Path $InstallDir -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -in 'fleet.old.exe', 'fleet.rejected' -or $_.Name -like 'fleet-v*.new.*' } |
    Remove-Item -Force -ErrorAction SilentlyContinue
if ($TrayExe) { Copy-IfOther $TrayExe $TrayPath }

[Environment]::SetEnvironmentVariable('FLEET_DATA_DIR', $DataDir, 'User')
if ($Relay) { [Environment]::SetEnvironmentVariable('FLEET_RELAY', $Relay, 'User') }
Add-ToUserPath $InstallDir

# Make the values visible to the commands below in this session too.
$env:FLEET_DATA_DIR = $DataDir
if ($Relay) { $env:FLEET_RELAY = $Relay }
$env:Path = "$InstallDir;$env:Path"

# The launcher: restart `fleet node` whenever it exits, with output appended
# to a log file that is rotated at 20 MB.
#
# It is also what lets the node update itself (FLEET_SELF_UPDATE=1: a node that
# exits needs something to start the next binary), and its safety net. A node
# updates itself by moving its running binary aside to fleet.old.exe, putting the
# new release in its place and exiting; this loop then starts the new one. If the
# new one keeps dying at startup (three runs of under a minute in a row), the
# launcher puts fleet.old.exe back and writes the bad release's version to
# fleet.rejected, so the node does not take that release again (the next one,
# it does). If it exits with 87 because it never reached the orchestrator, the
# previous binary goes back too, but the release is not rejected: the machine
# may only have been offline, and the node tries it again later. The count
# restarts whenever fleet.exe itself changes: the old binary exiting right after
# it updated itself is a success, not a strike against the new one.
$launcherBody = @'
$ErrorActionPreference = 'Continue'
$env:FLEET_SELF_UPDATE = '1'
$exe = Join-Path $PSScriptRoot 'fleet.exe'
$old = Join-Path $PSScriptRoot 'fleet.old.exe'
$rejected = Join-Path $PSScriptRoot 'fleet.rejected'
$logDir = Join-Path $env:LOCALAPPDATA 'fleet\logs'
$log = Join-Path $logDir 'node.log'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
# Puts the previous binary back, first noting which release is being rejected
# (its own --version, given 15 s to answer) so the node will not take it again.
function Restore-Previous([string]$why, [bool]$reject) {
    Add-Content -Path $log -Value "=== $(Get-Date -Format o) $why; putting the previous binary back"
    $out = Join-Path $env:TEMP 'fleet-rejected-version.txt'
    if ($reject) { try {
        $p = Start-Process -FilePath $exe -ArgumentList '--version' -NoNewWindow -PassThru -RedirectStandardOutput $out
        if ($p.WaitForExit(15000)) {
            $says = (Get-Content $out -Raw -ErrorAction SilentlyContinue)
            if ($says) { Set-Content -Path $rejected -Value $says.Trim() }
        } else { $p.Kill() }
    } catch {} }
    Move-Item -Force $old $exe
}
$quick = 0
$stamp = $null
while ($true) {
    if ((Test-Path $log) -and ((Get-Item $log).Length -gt 20MB)) {
        Move-Item -Force $log "$log.1"
    }
    # An update that failed halfway may leave no fleet.exe at all.
    if (-not (Test-Path $exe) -and (Test-Path $old)) {
        Add-Content -Path $log -Value "=== $(Get-Date -Format o) fleet.exe is missing; putting the previous binary back"
        Move-Item -Force $old $exe
    }
    $now = (Get-Item $exe -ErrorAction SilentlyContinue).LastWriteTimeUtc
    if ($now -ne $stamp) { $quick = 0; $stamp = $now }
    Add-Content -Path $log -Value "=== $(Get-Date -Format o) starting fleet node"
    $started = Get-Date
    # cmd's redirection appends the node's bytes as is; PowerShell 5.1's *>>
    # would re-encode them as UTF-16 and wrap stderr lines in error records.
    & cmd.exe /d /c "`"$exe`" node >> `"$log`" 2>&1"
    $code = $LASTEXITCODE
    if (((Get-Date) - $started).TotalSeconds -lt 60) { $quick++ } else { $quick = 0 }
    if ($code -eq 87 -and (Test-Path $old)) {
        Restore-Previous 'fleet.exe never reached the orchestrator after an update' $false
        $quick = 0
    } elseif ($quick -ge 3 -and (Test-Path $old)) {
        Restore-Previous 'fleet.exe died within a minute 3 times in a row' $true
        $quick = 0
    }
    Add-Content -Path $log -Value "=== $(Get-Date -Format o) fleet node exited with code $code; restarting in 5 s"
    Start-Sleep -Seconds 5
}
'@
Set-Content -Path $Launcher -Value $launcherBody -Encoding UTF8

if ($Ticket) {
    # `fleet join` admits this node and then keeps running as the node, so it
    # never returns on success. Run it only until the orchestrator has accepted
    # the key (the "connected to orchestrator" log line), then stop it: from
    # then on the scheduled task's `fleet node` connects with the known key. A
    # refused or already-used invite makes join exit early with an error.
    Write-Host 'Joining the fleet...'
    $joinLog = Join-Path $LogDir 'join.log'
    $proc = Start-Process -FilePath 'cmd.exe' -WindowStyle Hidden -PassThru `
        -ArgumentList '/d', '/c', "`"`"$ExePath`" join $Ticket > `"$joinLog`" 2>&1`""
    $deadline = (Get-Date).AddSeconds(90)
    $joined = $false
    while ((Get-Date) -lt $deadline -and -not $proc.HasExited) {
        Start-Sleep -Seconds 1
        if ((Test-Path $joinLog) -and (Select-String -Path $joinLog -Pattern 'connected to orchestrator' -Quiet)) {
            $joined = $true
            break
        }
    }
    # Stopping fleet.exe also ends its cmd.exe wrapper; Stop-Process covers
    # the case where the wrapper is still around.
    Stop-FleetNode
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    if (-not $joined) {
        $tail = if (Test-Path $joinLog) { (Get-Content $joinLog -Tail 15) -join "`n" } else { '' }
        throw "fleet join did not connect to the orchestrator. Output ($joinLog):`n$tail"
    }
    Write-Host 'Joined.'
}

# The launcher runs under a headless console host: it has a console (the node and
# cmd's log redirection need one) but no window. `powershell -WindowStyle Hidden`
# alone is not enough: when Windows Terminal is the default terminal app, it opens
# a visible Terminal window, and closing that window kills the node. Builds before
# Windows 10 1809 have no headless conhost and keep the old (briefly flashing) way.
$psArgs = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$Launcher`""
if ([Environment]::OSVersion.Version.Build -ge 17763) {
    $action = New-ScheduledTaskAction -Execute 'conhost.exe' `
        -Argument "--headless powershell.exe $psArgs" -WorkingDirectory $env:USERPROFILE
} else {
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
        -Argument $psArgs -WorkingDirectory $env:USERPROFILE
}
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $UserId
# Interactive + Limited: the user's own, non-elevated token, as in a terminal.
$principal = New-ScheduledTaskPrincipal -UserId $UserId -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -MultipleInstances IgnoreNew `
    -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName $TaskName -TaskPath $TaskPath `
    -Action $action -Trigger $trigger -Principal $principal -Settings $settings `
    -Description 'Runs `fleet node` (Claude Code agent host) as this user. Installed by deploy/windows/install-node.ps1.' `
    -Force | Out-Null

Start-ScheduledTask -TaskName $TaskName -TaskPath $TaskPath

Write-Host ''
Write-Host "Installed: $ExePath"
if (Test-Path $TrayPath) {
    Write-Host "Tray app:  $TrayPath (started by the node; set FLEET_NO_TRAY=1 to turn it off)"
}
Write-Host "Task:      Task Scheduler > $TaskPath$TaskName (at logon of $UserId)"
Write-Host "Data:      $DataDir"
Write-Host "Logs:      $(Join-Path $LogDir 'node.log')"
Write-Host 'Open a new terminal to pick up PATH/FLEET_* changes. Uninstall with -Uninstall.'
