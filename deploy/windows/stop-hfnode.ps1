<#
.SYNOPSIS
Stops the node, however it was started, and makes sure the radio is on receive.

.DESCRIPTION
Ends the scheduled task from install-task.ps1 and any hfnode-supervise.ps1 started
by hand (which would otherwise start the node again), then the node itself, then
runs `hfnode radio ... rx`, which stops the keyer and confirms receive, as the
systemd unit's stop hook does on Linux. The node is ended without its own stop, so
the radio may first finish the text already in its keyer. Pressing Ctrl-C in the
node's window is gentler: the node then puts the radio on receive before it exits.
#>
param(
    [string] $Dir = (Join-Path $env:LOCALAPPDATA "hfnode"),
    [string] $Hfnode = (Join-Path $env:USERPROFILE ".cargo\bin\hfnode.exe")
)
$config = Join-Path $Dir "hfnode.toml"

function Get-Supervisors {
    @(Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe' OR Name = 'pwsh.exe'" |
        Where-Object { $_.CommandLine -like "*hfnode-supervise.ps1*" })
}

Stop-ScheduledTask -TaskName "hfnode" -ErrorAction SilentlyContinue
Get-Supervisors | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Get-Process -Name "hfnode" -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1
$left = Get-Supervisors
& $Hfnode radio --config $config rx
$rx = $LASTEXITCODE
if ($left.Count -gt 0) {
    Write-Host "NOT stopped: hfnode-supervise.ps1 is still running (process $($left.ProcessId -join ', ')); end it and run this again"
    exit 1
}
if ($rx -eq 0) {
    Write-Host "stopped; radio confirmed on receive"
} else {
    Write-Host "stopped; could NOT confirm the radio is on receive, check it"
    exit 1
}
