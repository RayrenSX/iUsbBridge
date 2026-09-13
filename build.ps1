[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',
    [switch]$BridgeOnly,
    [switch]$SkipTests,
    [string]$BridgeOutputPath
)

$ErrorActionPreference = 'Stop'
$Root = [IO.Path]::GetFullPath($PSScriptRoot)
$Manifest = Join-Path $Root 'Cargo.toml'
$VendorRoot = Join-Path $Root 'vendor\idevice'
$VendorMarker = Join-Path $VendorRoot '.iusbbridge-vendor'
$VendorPatch = Join-Path $Root 'patches\idevice-compat.patch'
$ExpectedVendorCommit = 'e98264c4194e6980173c576ac79a58adce95492b'
$VendorRepository = 'https://github.com/jkcoxson/idevice.git'
$Dist = Join-Path $Root 'dist'
$Bridge = Join-Path $Dist 'iUsbBridge.exe'
$RuntimeManifest = Join-Path $Dist 'iUsbBridge.runtime.json'

function Invoke-Checked {
    param(
        [Parameter(Mandatory)][scriptblock]$Command,
        [Parameter(Mandatory)][string]$FailureMessage
    )

    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$FailureMessage Exit code: $LASTEXITCODE"
    }
}

function Initialize-IdeviceVendor {
    if (Test-Path -LiteralPath $VendorMarker -PathType Leaf) {
        $marker = (Get-Content -LiteralPath $VendorMarker -Raw).Trim()
        if ($marker -eq $ExpectedVendorCommit -and
            (Test-Path -LiteralPath (Join-Path $VendorRoot 'idevice\Cargo.toml') -PathType Leaf)) {
            return
        }
    }

    if (Test-Path -LiteralPath $VendorRoot) {
        throw "The generated vendor directory is incomplete or stale: $VendorRoot. Remove it and run the build again."
    }

    $git = @(Get-Command git -CommandType Application -ErrorAction Stop |
        Select-Object -First 1)[0]
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $VendorRoot) | Out-Null
    Invoke-Checked -FailureMessage 'Failed to clone the idevice dependency.' -Command {
        & $git.Source clone --filter=blob:none --no-checkout $VendorRepository $VendorRoot
    }
    Invoke-Checked -FailureMessage 'Failed to check out the pinned idevice dependency.' -Command {
        & $git.Source -C $VendorRoot checkout --detach $ExpectedVendorCommit
    }
    Invoke-Checked -FailureMessage 'Failed to apply the iUsbBridge idevice compatibility patch.' -Command {
        & $git.Source -C $VendorRoot apply --whitespace=nowarn $VendorPatch
    }
    [IO.File]::WriteAllText($VendorMarker, $ExpectedVendorCommit + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false))
}

if (-not (Test-Path -LiteralPath $Manifest -PathType Leaf)) {
    throw "Rust manifest is missing: $Manifest"
}
if (-not (Test-Path -LiteralPath $VendorPatch -PathType Leaf)) {
    throw "idevice compatibility patch is missing: $VendorPatch"
}

Initialize-IdeviceVendor

$cargo = @(Get-Command cargo -CommandType Application -ErrorAction Stop |
    Select-Object -First 1)[0]
if (-not $SkipTests) {
    Invoke-Checked -FailureMessage 'iUsbBridge tests failed.' -Command {
        & $cargo.Source test --locked --manifest-path $Manifest
    }
}

$cargoArguments = @('build', '--locked', '--manifest-path', $Manifest)
if ($Configuration -eq 'Release') {
    $cargoArguments += '--release'
}
Invoke-Checked -FailureMessage 'iUsbBridge build failed.' -Command {
    & $cargo.Source @cargoArguments
}

$profileDirectory = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
$builtBridge = Join-Path $Root "target\$profileDirectory\iphone-mirror-idevice-bridge.exe"
if (-not (Test-Path -LiteralPath $builtBridge -PathType Leaf)) {
    throw "Rust build output is missing: $builtBridge"
}

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
Copy-Item -LiteralPath $builtBridge -Destination $Bridge -Force
$hash = (Get-FileHash -LiteralPath $Bridge -Algorithm SHA256).Hash.ToLowerInvariant()
$manifestObject = [ordered]@{
    schema = 2
    backend = 'idevice'
    files = @([ordered]@{ path = 'iUsbBridge.exe'; sha256 = $hash })
}
[IO.File]::WriteAllText($RuntimeManifest,
    ($manifestObject | ConvertTo-Json -Depth 4) + [Environment]::NewLine,
    [Text.UTF8Encoding]::new($false))

if (-not [string]::IsNullOrWhiteSpace($BridgeOutputPath)) {
    $BridgeOutputPath = [IO.Path]::GetFullPath($BridgeOutputPath)
    $destinationDirectory = Split-Path -Parent $BridgeOutputPath
    New-Item -ItemType Directory -Force -Path $destinationDirectory | Out-Null
    Copy-Item -LiteralPath $Bridge -Destination $BridgeOutputPath -Force
    Copy-Item -LiteralPath $RuntimeManifest `
        -Destination (Join-Path $destinationDirectory 'iUsbBridge.runtime.json') -Force
}

if (-not $BridgeOnly) {
    $demoOutput = Join-Path $Dist 'iUsbBridge-Demo'
    if (Test-Path -LiteralPath $demoOutput) {
        Remove-Item -LiteralPath $demoOutput -Recurse -Force
    }
    Invoke-Checked -FailureMessage 'iUsbBridge demo build failed.' -Command {
        dotnet publish (Join-Path $Root 'demo\iUsbBridgeDemo.csproj') `
            -c $Configuration -r win-x64 --self-contained true -o $demoOutput
    }
    Copy-Item -LiteralPath $Bridge -Destination (Join-Path $demoOutput 'iUsbBridge.exe') -Force
    Copy-Item -LiteralPath $RuntimeManifest `
        -Destination (Join-Path $demoOutput 'iUsbBridge.runtime.json') -Force
    Write-Host "iUsbBridge demo package: $demoOutput"
}

Write-Host "iUsbBridge: $Bridge"
Write-Host "SHA-256: $hash"
