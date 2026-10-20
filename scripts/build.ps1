
# Native Windows build entry point. Example: ./scripts/build.ps1 --debug
$ErrorActionPreference = 'Stop'
$interpreter = if ($env:PYTHON) { $env:PYTHON } else { 'python' }
& $interpreter (Join-Path $PSScriptRoot 'build.py') @args
exit $LASTEXITCODE
