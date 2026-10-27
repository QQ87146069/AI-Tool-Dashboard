# Dot-source to use the project-local portable toolchain; no persistent PATH edits.
$env:CARGO_HOME = Join-Path $PSScriptRoot '..\.tools\cargo'
$env:RUSTUP_HOME = Join-Path $PSScriptRoot '..\.tools\rustup'
$env:PATH = "$(Join-Path $env:CARGO_HOME 'bin');$(Join-Path $PSScriptRoot '..\.tools\mingw64\bin');$env:PATH"
