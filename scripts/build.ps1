param(
    [ValidateSet('debug', 'release')]
    [string]$Profile = 'release',
    [switch]$ReleaseAssets
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ProjectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
Push-Location $ProjectRoot
try {
    if ($ReleaseAssets -and $Profile -ne 'release') {
        throw '发布产物必须使用 release 配置'
    }

    $MetadataJson = & cargo metadata --no-deps --format-version 1 --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed: $LASTEXITCODE"
    }
    $Metadata = $MetadataJson | ConvertFrom-Json
    $Package = @($Metadata.packages | Where-Object { $_.name -eq 'cpa-devin' })[0]
    $Version = $Package.version

    & cargo test --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo test failed: $LASTEXITCODE"
    }

    $BuildArguments = @('build', '--locked')
    if ($Profile -eq 'release') {
        $BuildArguments += '--release'
    }
    & cargo @BuildArguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed: $LASTEXITCODE"
    }

    $Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
    $GoArchitecture = switch ($Architecture) {
        'x64' { 'amd64' }
        'arm64' { 'arm64' }
        default { $Architecture }
    }
    if ($IsWindows) {
        $Platform = "windows-$Architecture"
        $GoOS = 'windows'
        $Library = 'cpa_devin.dll'
        $InstalledName = 'cpa-devin.dll'
    }
    elseif ($IsMacOS) {
        $Platform = "macos-$Architecture"
        $GoOS = 'darwin'
        $Library = 'libcpa_devin.dylib'
        $InstalledName = 'cpa-devin.dylib'
    }
    else {
        $Platform = "linux-$Architecture"
        $GoOS = 'linux'
        $Library = 'libcpa_devin.so'
        $InstalledName = 'cpa-devin.so'
    }

    $PackagePath = Join-Path $ProjectRoot "dist/cpa-devin-$Platform"
    New-Item -ItemType Directory -Path $PackagePath -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $ProjectRoot "target/$Profile/$Library") -Destination (Join-Path $PackagePath $InstalledName) -Force
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'README.md') -Destination $PackagePath -Force
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'LICENSE') -Destination $PackagePath -Force

    $Hash = Get-FileHash -LiteralPath (Join-Path $PackagePath $InstalledName) -Algorithm SHA256
    "$($Hash.Hash.ToLowerInvariant())  $InstalledName" | Set-Content -LiteralPath (Join-Path $PackagePath 'SHA256SUMS.txt') -Encoding utf8NoBOM

    $AssetName = if ($ReleaseAssets) {
        "cpa-devin_{0}_{1}_{2}" -f $Version, $GoOS, $GoArchitecture
    }
    else {
        "cpa-devin-$Platform"
    }
    $ArchivePath = Join-Path $ProjectRoot "dist/$AssetName.zip"
    Compress-Archive -Path (Join-Path $PackagePath '*') -DestinationPath $ArchivePath -Force
    Write-Output "安装包已生成：$ArchivePath"
}
finally {
    Pop-Location
}
