[CmdletBinding()]
param(
    [string]$Python = "python",
    [string]$Configuration = "Release",
    [switch]$BridgeOnly,
    [string]$BridgeOutputPath,
    [string]$EnvironmentPath,
    [string]$NativeDirectory
)

$ErrorActionPreference = "Stop"
$Root = [IO.Path]::GetFullPath($PSScriptRoot)
$PythonDist = Join-Path $Root "dist"
$PythonBuild = Join-Path $Root "build\python"
$Out = Join-Path $PythonDist "iUsbBridge-Demo"
$BridgeDirectory = Join-Path $PythonDist "iUsbBridge"
$Bridge = Join-Path $BridgeDirectory "iUsbBridge.exe"
$Spec = Join-Path $Root "iUsbBridge.spec"
$Source = Join-Path $Root "src\usb_touch_bridge.py"
$Requirements = Join-Path $Root "requirements.txt"
if ([string]::IsNullOrWhiteSpace($EnvironmentPath)) {
    $EnvironmentPath = Join-Path $Root "work\usb-touch-bridge-python"
}
$EnvironmentPath = [IO.Path]::GetFullPath($EnvironmentPath)
$EnvironmentPython = Join-Path $EnvironmentPath "Scripts\python.exe"
if ([string]::IsNullOrWhiteSpace($NativeDirectory)) {
    $NativeDirectory = Join-Path $Root 'native'
}
$NativeDirectory = [IO.Path]::GetFullPath($NativeDirectory)
# Use the same pinned Windows x64 backends as the iPhoneMirror Python build.
# Native files are supplied by the caller and are never committed here.
$nativeHashes = @{
    'libusb0.dll' = '4F18B5D2C28AA66B648C8683C6D09B52B92CBBEE85984BBEFAD5F38A64BC2A14'
    'libusb-1.0.dll' = '5072054CB3002AE071F382AD5C2C2B0092D9451C537D0C13444C2B6F968F7251'
}
foreach ($name in $nativeHashes.Keys) {
    $nativePath = Join-Path $NativeDirectory $name
    if (-not (Test-Path -LiteralPath $nativePath -PathType Leaf)) {
        throw "Pinned USB backend is missing: $nativePath. Supply -NativeDirectory containing both x64 DLLs."
    }
    if ((Get-FileHash -LiteralPath $nativePath -Algorithm SHA256).Hash -ne $nativeHashes[$name]) {
        throw "Pinned USB backend hash mismatch: $nativePath"
    }
}

foreach ($required in @($Spec, $Source, $Requirements)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "USB control build input is missing: $required"
    }
}

function Write-BridgeRuntimeManifest([string]$Directory) {
    $fullDirectory = [IO.Path]::GetFullPath($Directory).TrimEnd('\')
    $manifestPath = Join-Path $fullDirectory 'iUsbBridge.runtime.json'
    if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
        Remove-Item -LiteralPath $manifestPath -Force
    }
    $files = @(Get-ChildItem -LiteralPath $fullDirectory -Recurse -File |
        Sort-Object FullName | ForEach-Object {
            $relative = $_.FullName.Substring($fullDirectory.Length + 1) -replace '\\', '/'
            [PSCustomObject][ordered]@{
                path = $relative
                sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        })
    if ($files.Count -eq 0 -or
        @($files | Where-Object { $_.path -eq 'iUsbBridge.exe' }).Count -ne 1 -or
        @($files | Where-Object { $_.path -like '_internal/*' }).Count -eq 0) {
        throw 'PyInstaller onedir output does not contain the bridge and its runtime directory.'
    }
    $manifest = [PSCustomObject][ordered]@{
        schema = 1
        files = $files
    }
    [IO.File]::WriteAllText($manifestPath,
        ($manifest | ConvertTo-Json -Depth 4), [Text.UTF8Encoding]::new($false))
}

function Assert-SafeDirectory([string]$Path, [string]$ExpectedRoot) {
    $fullPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $rootPath = [IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\')
    if (-not $fullPath.StartsWith($rootPath + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing recursive mutation outside $rootPath`: $fullPath"
    }
    $ancestor = [IO.DirectoryInfo]::new($fullPath)
    while ($null -ne $ancestor) {
        if ($ancestor.Exists -and ($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Refusing recursive mutation through a reparse point: $($ancestor.FullName)"
        }
        $ancestor = $ancestor.Parent
    }
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return }
    $directory = Get-Item -LiteralPath $Path -Force
    if (($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing recursive mutation through a reparse point: $($directory.FullName)"
    }
    $reparse = @(Get-ChildItem -LiteralPath $Path -Recurse -Force |
        Where-Object {
            ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
        })
    if ($reparse.Count -ne 0) {
        throw "Refusing recursive mutation through a reparse point: $($reparse[0].FullName)"
    }
}

$bootstrapPython = @(Get-Command $Python -CommandType Application -ErrorAction Stop |
    Select-Object -First 1)[0]
if (-not (Test-Path -LiteralPath $EnvironmentPython -PathType Leaf)) {
    & $bootstrapPython.Source -m venv $EnvironmentPath
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to create USB touch bridge Python environment: $LASTEXITCODE"
    }
}
if (-not (Test-Path -LiteralPath $EnvironmentPython -PathType Leaf)) {
    throw "USB touch bridge Python environment is incomplete: $EnvironmentPython"
}

& $EnvironmentPython -m pip install --disable-pip-version-check --requirement $Requirements
if ($LASTEXITCODE -ne 0) {
    throw "Failed to install USB touch bridge build requirements: $LASTEXITCODE"
}

$previousNativeDirectory = $env:IUSBBRIDGE_NATIVE_DIRECTORY
$env:IUSBBRIDGE_NATIVE_DIRECTORY = $NativeDirectory
Push-Location $Root
try {
    Assert-SafeDirectory $BridgeDirectory $PythonDist
    Assert-SafeDirectory $PythonBuild $Root
    & $EnvironmentPython -m PyInstaller --noconfirm --clean --distpath $PythonDist --workpath $PythonBuild $Spec
    if ($LASTEXITCODE -ne 0) { throw "PyInstaller failed: $LASTEXITCODE" }

    if (-not (Test-Path -LiteralPath $Bridge -PathType Leaf) -or
        -not (Test-Path -LiteralPath (Join-Path $BridgeDirectory '_internal') -PathType Container)) {
        throw "PyInstaller did not produce the USB touch bridge: $Bridge"
    }
    Write-BridgeRuntimeManifest $BridgeDirectory
    $runtimeOutput = & $Bridge --check-runtime
    if ($LASTEXITCODE -ne 0) { throw 'Packaged USB bridge runtime dependency check failed.' }
    $runtimeReport = ($runtimeOutput -join "`n") | ConvertFrom-Json
    if ($runtimeReport.runtime -ne 'ready' -or
        'pymobiledevice3.services.cryptexd' -notin @($runtimeReport.modules) -or
        'cryptex_owned_channels' -notin @($runtimeReport.functional_checks)) {
        throw 'Packaged USB bridge is missing the required Cryptex lifecycle capability.'
    }
    Write-Output ($runtimeOutput -join "`n")

    if (-not [string]::IsNullOrWhiteSpace($BridgeOutputPath)) {
        $BridgeOutputPath = [IO.Path]::GetFullPath($BridgeOutputPath)
        $destinationDirectory = Split-Path -Parent $BridgeOutputPath
        $destinationRuntimeDirectory = Join-Path $destinationDirectory '_internal'
        New-Item -ItemType Directory -Force -Path $destinationDirectory | Out-Null
        if (Test-Path -LiteralPath $destinationRuntimeDirectory) {
            Assert-SafeDirectory $destinationRuntimeDirectory $destinationDirectory
            Remove-Item -LiteralPath $destinationRuntimeDirectory -Recurse -Force
        }
        Copy-Item -LiteralPath $Bridge -Destination $BridgeOutputPath -Force
        Copy-Item -LiteralPath (Join-Path $BridgeDirectory '_internal') `
            -Destination $destinationRuntimeDirectory -Recurse -Force
        Copy-Item -LiteralPath (Join-Path $BridgeDirectory 'iUsbBridge.runtime.json') `
            -Destination (Join-Path $destinationDirectory 'iUsbBridge.runtime.json') -Force
    }

    if ($BridgeOnly) {
        Write-Host "USB touch bridge: $Bridge"
        return
    }

    if (Test-Path -LiteralPath $Out) {
        Assert-SafeDirectory $Out $PythonDist
        Remove-Item -LiteralPath $Out -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $Out | Out-Null
    Get-ChildItem -LiteralPath $BridgeDirectory | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $Out -Recurse -Force
    }

    Push-Location (Join-Path $Root "demo")
    try {
        dotnet publish -c $Configuration -r win-x64 --self-contained true -o $Out
        if ($LASTEXITCODE -ne 0) { throw "dotnet publish failed: $LASTEXITCODE" }
    }
    finally { Pop-Location }

    Write-Host "USB control package: $Out"
}
finally {
    Pop-Location
    if ($null -eq $previousNativeDirectory) {
        Remove-Item Env:IUSBBRIDGE_NATIVE_DIRECTORY -ErrorAction SilentlyContinue
    }
    else { $env:IUSBBRIDGE_NATIVE_DIRECTORY = $previousNativeDirectory }
}
