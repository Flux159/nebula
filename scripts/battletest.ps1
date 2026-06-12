# Battle-test entry point (Windows). Usage: scripts\battletest.ps1 balloon --quick
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

cargo build -p nebula-cli -p nebulad -p nebula-battletest
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$env:NEBULA_BATTLETEST = "1"
& "target\debug\nebula-battletest.exe" @args
exit $LASTEXITCODE
