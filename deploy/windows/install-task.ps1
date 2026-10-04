<#
.SYNOPSIS
Starts hfnode-supervise.ps1 at log-on with Task Scheduler. See docs/windows-setup.md.

.DESCRIPTION
Registers a scheduled task, "hfnode", for the current user that runs the supervisor
in a console window when that user logs on. It runs in the user's session, the way
Windows' microphone permission (which covers the radio's USB sound card) expects;
running hfnode as a Windows service is not supported. The window stays open after
the supervisor ends, so that its last lines can be read, and the supervisor's own
lines also go to supervise.log in the node's folder.

To stop the node, press Ctrl-C in that window, or run stop-hfnode.ps1. To remove
the task: Unregister-ScheduledTask -TaskName hfnode.

Do not run this until docs/hardware-test-plan.md has been worked through.
#>
param(
    [string] $Dir = (Join-Path $env:LOCALAPPDATA "hfnode"),
    # Where `cargo install --locked --path crates/hfnode` puts it.
    [string] $Hfnode = (Join-Path $env:USERPROFILE ".cargo\bin\hfnode.exe")
)
$ErrorActionPreference = "Stop"
$supervise = Join-Path $PSScriptRoot "hfnode-supervise.ps1"
$config = Join-Path $Dir "hfnode.toml"
$envFile = Join-Path $Dir "env"
$logFile = Join-Path $Dir "supervise.log"
foreach ($f in @($supervise, $Hfnode, $config, $envFile)) {
    if (-not (Test-Path -LiteralPath $f)) { throw "missing $f (see docs/windows-setup.md)" }
}
# -NoExit keeps the window open once the supervisor ends (gave up, or stopped).
$arguments = "-NoExit -NoProfile -ExecutionPolicy Bypass -File `"$supervise`" " +
    "-Hfnode `"$Hfnode`" -Config `"$config`" -EnvFile `"$envFile`" -LogFile `"$logFile`""
$action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $arguments -WorkingDirectory $Dir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
# No time limit, no restarts by Task Scheduler (the supervisor does that, with a
# limit), and keep going on battery. Priority 4 is normal priority; Task Scheduler's
# default, 7, would run the node (decoding and keying, both timed) below normal.
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew `
    -Priority 4
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive
Register-ScheduledTask -TaskName "hfnode" -Action $action -Trigger $trigger `
    -Settings $settings -Principal $principal -Force | Out-Null
Write-Host "registered task hfnode: starts at your next log-on, or now with Start-ScheduledTask -TaskName hfnode"
