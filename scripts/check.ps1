# Everything that must pass before merging to main (Windows).
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
function Run($cmd) { Write-Host "> $cmd"; Invoke-Expression $cmd; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }
Run "cargo fmt --all --check"
Run "cargo clippy --workspace --all-targets -- -D warnings"
Run "cargo test --workspace"
if (Get-Command cargo-deny -ErrorAction SilentlyContinue) { Run "cargo deny check" } else { Write-Host "cargo-deny not installed: skipped license check" }
Write-Host "All checks passed."
