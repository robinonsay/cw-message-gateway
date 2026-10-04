<#
.SYNOPSIS
Stops the node started by install-task.ps1 and makes sure the radio is on receive.

.DESCRIPTION
Ending the scheduled task stops the supervisor; the node itself is then stopped too,
and `hfnode radio ... rx` stops the keyer and confirms receive, as the systemd unit's
stop hook does on Linux. Pressing Ctrl-C in the node's window is gentler: the node
then puts the radio on receive before it exits.
#>
param(
    [string] $Dir = (Join-Path $env:LOCALAPPDATA "hfnode"),
    [string] $Hfnode = (Join-Path $env:USERPROFILE ".cargo\bin\hfnode.exe")
)
$config = Join-Path $Dir "hfnode.toml"
Stop-ScheduledTask -TaskName "hfnode" -ErrorAction SilentlyContinue
Get-Process -Name "hfnode" -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1
& $Hfnode radio --config $config rx
if ($LASTEXITCODE -eq 0) {
    Write-Host "stopped; radio confirmed on receive"
} else {
    Write-Host "stopped; could NOT confirm the radio is on receive, check it"
    exit 1
}
