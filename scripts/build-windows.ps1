# Build multilink natively on Windows and bundle it with wintun.dll, which
# multilink.exe needs next to it at runtime.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
#
# Needs: Rust (rustup) with the MSVC toolchain. Builds for this machine's architecture.
# Output: target\windows-<arch>\{multilink.exe, wintun.dll, wintun-LICENSE.txt, multilink.example.toml}
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is very slow with the progress bar

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'ARM64' { 'arm64' }
    'AMD64' { 'amd64' }
    default { throw "unsupported architecture $env:PROCESSOR_ARCHITECTURE" }
}
$dir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Official wintun release, pinned by version and SHA-256 (as published on wintun.net).
$wintunVersion = '0.14.1'
$wintunSha256 = '07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51'
$zip = Join-Path $dir "wintun-$wintunVersion.zip"
if (-not (Test-Path $zip)) {
    # Windows PowerShell 5.1 may not offer TLS 1.2 by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest "https://www.wintun.net/builds/wintun-$wintunVersion.zip" -OutFile "$zip.tmp"
    Move-Item "$zip.tmp" $zip
}
$sum = (Get-FileHash $zip -Algorithm SHA256).Hash
if ($sum -ne $wintunSha256) {
    Remove-Item $zip
    throw "wintun zip checksum mismatch: got $sum"
}

# .NET resolves relative paths against the process directory, not PowerShell's: use full paths.
$out = (New-Item -ItemType Directory -Force (Join-Path $dir "windows-$arch")).FullName
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead((Resolve-Path $zip).Path)
try {
    foreach ($pair in @(@("wintun/bin/$arch/wintun.dll", 'wintun.dll'), @('wintun/LICENSE.txt', 'wintun-LICENSE.txt'))) {
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($archive.GetEntry($pair[0]), (Join-Path $out $pair[1]), $true)
    }
} finally {
    $archive.Dispose()
}
Copy-Item (Join-Path $dir 'release\multilink.exe'), 'multilink.example.toml' $out
Write-Host "Bundle ready: $out (run multilink.exe from an Administrator prompt)"
