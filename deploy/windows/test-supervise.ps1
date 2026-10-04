# Checks hfnode-supervise.ps1 against a stand-in for hfnode.exe (run by CI on
# Windows): a failing node is restarted then given up on, a clean stop is left
# alone, every exit is followed by the receive check, and secrets are read from the
# env file without running it. Exits non-zero on any failure.
param([string] $Hfnode = "")

$ErrorActionPreference = "Continue"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$supervise = Join-Path $here "hfnode-supervise.ps1"
$work = Join-Path ([IO.Path]::GetTempPath()) ("hfnode-supervise-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
$fake = Join-Path $work "hfnode.cmd"
$log = Join-Path $work "log"
Set-Content -LiteralPath $fake -Encoding ASCII -Value @'
@echo off
>>"%FAKE_LOG%" echo %*
if "%1"=="run" (
    >>"%FAKE_LOG%" echo pw=%HFNODE_EMAIL_PASSWORD%
    exit /b %FAKE_RUN_CODE%
)
exit /b 0
'@
$failures = 0
function Check([bool] $ok, [string] $what) {
    if ($ok) { Write-Host "ok: $what" } else { Write-Host "FAIL: $what"; $script:failures++ }
}
function Run-Supervisor([string] $code, [string] $envFile, [string] $exe) {
    Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue
    $env:FAKE_LOG = $log
    $env:FAKE_RUN_CODE = $code
    $argv = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $supervise,
        "-Hfnode", $exe, "-Config", "node.toml", "-RestartSec", "0")
    if ($envFile) { $argv += @("-EnvFile", $envFile) }
    & powershell.exe @argv | Out-Host
    return $LASTEXITCODE
}
function Count([string] $line) {
    if (-not (Test-Path -LiteralPath $log)) { return 0 }
    return @(Get-Content -LiteralPath $log | Where-Object { $_.Trim() -eq $line }).Count
}

$code = Run-Supervisor "1" "" $fake
Check ($code -eq 1) "a failing node: supervisor exits 1 (got $code)"
Check ((Count "run --config node.toml") -eq 3) "a failing node is started 3 times"
Check ((Count "radio --config node.toml rx") -eq 3) "receive is checked after each exit"

$envFile = Join-Path $work "env"
$marker = Join-Path $work "ran"
Set-Content -LiteralPath $envFile -Value @(
    "# node secrets", "", "HFNODE_EMAIL_PASSWORD=pa=ss word", "BAD-KEY=x",
    "`$(New-Item -ItemType File -Path '$marker')")
$code = Run-Supervisor "0" $envFile $fake
Check ($code -eq 0) "a clean stop: supervisor exits 0 (got $code)"
Check ((Count "run --config node.toml") -eq 1) "a clean stop is not restarted"
Check ((Count "radio --config node.toml rx") -eq 1) "receive is checked after a clean stop"
Check ((Count "pw=pa=ss word") -eq 1) "the env file's password reaches the node"
Check (-not (Test-Path -LiteralPath $marker)) "nothing in the env file is run"

if ($Hfnode) {
    # The real binary, with a config that does not exist: fails at once.
    $code = Run-Supervisor "" "" $Hfnode
    Check ($code -eq 1) "the real hfnode failing to start is given up on (got $code)"
}

Remove-Item -LiteralPath $work -Recurse -Force
if ($failures -gt 0) { Write-Host "$failures check(s) failed"; exit 1 }
Write-Host "all checks passed"
exit 0
