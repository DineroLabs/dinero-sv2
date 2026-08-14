# Install the Dinero miner on Windows: resolves the latest miner-v* release,
# downloads the x86_64 binary + SHA256SUMS, verifies the hash, installs to
# %LOCALAPPDATA%\DineroMiner\bin\dinero-miner.exe and adds that dir to the
# user PATH. $env:DINERO_MINER_VERSION = "miner-vX.Y.Z" overrides "latest".
$ErrorActionPreference = "Stop"
$Repo   = "DineroLabs/dinero-sv2"
$Asset  = "dinero-sv2-miner-x86_64-pc-windows-msvc.exe"

$Tag = $env:DINERO_MINER_VERSION
if (-not $Tag) {
    $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases"
    $Tag = ($releases | Where-Object { $_.tag_name -like "miner-v*" } |
            Select-Object -First 1).tag_name
}
if (-not $Tag) { throw "no miner release found" }

$Base = "https://github.com/$Repo/releases/download/$Tag"
$Tmp  = Join-Path $env:TEMP ("dinero-miner-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    Invoke-WebRequest -Uri "$Base/$Asset"      -OutFile (Join-Path $Tmp "miner.exe")
    Invoke-WebRequest -Uri "$Base/SHA256SUMS"  -OutFile (Join-Path $Tmp "SHA256SUMS")

    $expected = (Get-Content (Join-Path $Tmp "SHA256SUMS") |
                 Where-Object { $_ -match [regex]::Escape($Asset) + '$' })
    if (-not $expected) { throw "SHA256SUMS has no entry for $Asset" }
    $expectedHash = ($expected -split '\s+')[0].ToLower()
    $actualHash = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp "miner.exe")).Hash.ToLower()
    if ($actualHash -ne $expectedHash) { throw "checksum mismatch: $actualHash != $expectedHash" }

    $BinDir = Join-Path $env:LOCALAPPDATA "DineroMiner\bin"
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Copy-Item (Join-Path $Tmp "miner.exe") (Join-Path $BinDir "dinero-miner.exe") -Force
    Write-Host "installed: $BinDir\dinero-miner.exe ($Tag)"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ';') -notcontains $BinDir) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$BinDir", "User")
        Write-Host "added to user PATH (open a new terminal to pick it up)"
    }
    Write-Host "run: dinero-miner"
} finally {
    Remove-Item -Recurse -Force $Tmp
}
