$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$projectRoot = Split-Path $PSScriptRoot -Parent
$toolsPath = Join-Path $projectRoot '.tools'
New-Item -ItemType Directory -Force $toolsPath | Out-Null
$env:CARGO_HOME = Join-Path $toolsPath 'cargo'
$env:RUSTUP_HOME = Join-Path $toolsPath 'rustup'
$env:RUSTUP_INIT_SKIP_PATH_CHECK = 'yes'

if (!(Test-Path (Join-Path $env:CARGO_HOME 'bin\rustup.exe'))) {
    $installer = Join-Path $toolsPath 'rustup-init.exe'
    Invoke-WebRequest 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe' -OutFile $installer
    & $installer -y --no-modify-path --profile minimal --default-host x86_64-pc-windows-gnu --default-toolchain 1.99.1
    if ($LASTEXITCODE -ne 0) { throw 'Rust installation failed' }
}

if (!(Test-Path (Join-Path $toolsPath 'mingw64\bin\gcc.exe'))) {
    $archive = Join-Path $toolsPath 'winlibs.zip'
    $url = 'https://github.com/brechtsanders/winlibs_mingw/releases/download/16.2.0posix-23.1.3-14.0.0-ucrt-r3/winlibs-x86_64-posix-seh-gcc-16.2.0-mingw-w64ucrt-14.0.0-r3.zip'
    Invoke-WebRequest $url -OutFile $archive
    $expected = '3c69305081e4a647144339a936244fcf7d3d9e558319e1ad7be2cd2886d4b6a1'
    if ((Get-FileHash $archive -Algorithm SHA256).Hash.ToLower() -ne $expected) {
        throw 'Compiler archive SHA256 mismatch'
    }
    tar -xf $archive -C $toolsPath
    if ($LASTEXITCODE -ne 0) { throw 'Compiler extraction failed' }
}
. (Join-Path $PSScriptRoot 'dev-env.ps1')
rustup component add rustfmt clippy
if ($LASTEXITCODE -ne 0) { throw 'Rust components installation failed' }
rustc --version
cargo --version
