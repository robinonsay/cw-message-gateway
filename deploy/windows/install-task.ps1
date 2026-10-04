<#
.SYNOPSIS
Starts hfnode-supervise.ps1 at log-on with Task Scheduler. See docs/windows-setup.md.

.DESCRIPTION
Registers a scheduled task, "hfnode", for the current user that runs the supervisor
in a console window when that user logs on. It runs in the user's session so that
it can use the radio's USB sound card (a Windows service could not).

To stop the node, press Ctrl-C in that window, or run stop-hfnode.ps1. To remove
the task: Unregister-ScheduledTask -TaskName hfnode.

Do not run this until docs/hardware-test-plan.md has been worked through.
#>
param(
    [string] $Dir = (Join-Path $env:LOCALAPPDATA "hfnode"),
    # Where `cargo install --path crates/hfnode` puts it.
    [string] $Hfnode = (Join-Path $env:USERPROFILE ".cargo\bin\hfnode.exe")
)
$ErrorActionPreference = "Stop"
$supervise = Join-Path $PSScriptRoot "hfnode-supervise.ps1"
$config = Join-Path $Dir "hfnode.toml"
$envFile = Join-Path $Dir "env"
foreach ($f in @($supervise, $Hfnode, $config, $envFile)) {
    if (-not (Test-Path -LiteralPath $f)) { throw "missing $f (see docs/windows-setup.md)" }
}
$arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$supervise`" " +
    "-Hfnode `"$Hfnode`" -Config `"$config`" -EnvFile `"$envFile`""
$action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $arguments -WorkingDirectory $Dir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
# No time limit, no restarts by Task Scheduler (the supervisor does that, with a
# limit), and keep going on battery.
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive
Register-ScheduledTask -TaskName "hfnode" -Action $action -Trigger $trigger `
    -Settings $settings -Principal $principal -Force | Out-Null
Write-Host "registered task hfnode: starts at your next log-on, or now with Start-ScheduledTask -TaskName hfnode"
