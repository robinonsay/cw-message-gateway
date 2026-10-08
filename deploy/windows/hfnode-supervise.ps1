<#
.SYNOPSIS
Keeps `hfnode run` going on Windows. See docs/windows-setup.md.

.DESCRIPTION
Does on Windows what deploy/hfnode.service has systemd do on Linux:

- restarts the node 30 s after it fails, and gives up after 3 starts within an
  hour, so that a radio that is off, unplugged or misbehaving does not get an
  endless loop of start-up tunes (each of which transmits);
- does not restart it after a clean stop (exit code 0);
- after every stop or crash runs `hfnode radio ... rx`, which stops the keyer and
  makes sure the radio is on receive (with the keyer box, that its key is open);
- loads secrets from an environment file (KEY=value lines, no quotes);
- keeps the computer from going to sleep while it runs.

Stop it with Ctrl-C in its window: the node puts the radio on receive itself, then
this script checks receive again. Closing the window, logging off or a restart
instead ends both at once, without either step (Windows does not wait for them);
then run stop-hfnode.ps1, which checks receive.

With -LogFile, this script's own lines (starts, exits, the receive checks, giving
up) are also appended to that file.

Do not set this to start automatically until docs/hardware-test-plan.md has been
worked through; `hfnode run` refuses to start before station.commissioned = "done".

.EXAMPLE
powershell -NoProfile -ExecutionPolicy Bypass -File hfnode-supervise.ps1 `
    -Hfnode "$env:USERPROFILE\.cargo\bin\hfnode.exe" `
    -Config "$env:LOCALAPPDATA\hfnode\hfnode.toml" `
    -EnvFile "$env:LOCALAPPDATA\hfnode\env"
#>
param(
    [Parameter(Mandatory = $true)] [string] $Hfnode,
    [Parameter(Mandatory = $true)] [string] $Config,
    [string] $EnvFile = "",
    [string] $LogFile = "",
    # The same limits as the systemd unit; settable for tests.
    [int] $RestartSec = 30,
    [int] $StartLimitBurst = 3,
    [int] $StartLimitIntervalSec = 3600
)

# Not "Stop": Windows PowerShell 5.1 can turn a native program's log lines on stderr
# into errors, which must not stop this script while the node runs.
$ErrorActionPreference = "Continue"

function Log([string] $Text) {
    $line = "{0} hfnode-supervise: {1}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $Text
    Write-Host $line
    if ($LogFile) { Add-Content -LiteralPath $LogFile -Value $line -ErrorAction SilentlyContinue }
}

if ($EnvFile) {
    if (-not (Test-Path -LiteralPath $EnvFile)) {
        Log "cannot read $EnvFile"
        exit 1
    }
    # Read KEY=value lines without running anything in the file.
    foreach ($line in Get-Content -LiteralPath $EnvFile) {
        if ($line -match '^\s*$' -or $line -match '^\s*#') { continue }
        if ($line -match '^([A-Za-z_][A-Za-z0-9_]*)=(.*)$') {
            [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], "Process")
        } else {
            Log "ignoring a line in $EnvFile that is not KEY=value"
        }
    }
}

# Keep the computer awake (ES_CONTINUOUS | ES_SYSTEM_REQUIRED) while this runs.
try {
    Add-Type -Namespace HfNode -Name Power -MemberDefinition @'
[DllImport("kernel32.dll")]
public static extern uint SetThreadExecutionState(uint esFlags);
'@
    [void][HfNode.Power]::SetThreadExecutionState([uint32]"0x80000001")
} catch {
    Log "could not keep the computer awake: $_"
}

# Put the radio on receive, whatever state the node left it in. Failing here (radio
# off, port gone) is not an error.
function Force-Receive {
    & $Hfnode radio --config $Config rx
    if ($LASTEXITCODE -eq 0) {
        Log "radio confirmed on receive"
    } else {
        Log "could not confirm the radio is on receive; check it"
    }
}

$starts = @()
while ($true) {
    $now = Get-Date
    $starts = @($starts | Where-Object { ($now - $_).TotalSeconds -lt $StartLimitIntervalSec })
    if ($starts.Count -ge $StartLimitBurst) {
        Log "gave up: $($starts.Count) starts within $StartLimitIntervalSec s. Fix the cause (see the log above), then start it again."
        exit 1
    }
    $starts += $now

    Log "starting: $Hfnode run --config $Config"
    $status = $null
    try {
        & $Hfnode run --config $Config
        $status = $LASTEXITCODE
    } finally {
        # Also when Ctrl-C stops this script.
        Log "hfnode exited with status $status"
        Force-Receive
    }
    if ($status -eq 0) {
        Log "clean stop; not restarting"
        exit 0
    }
    Log "restarting in $RestartSec s"
    Start-Sleep -Seconds $RestartSec
}
