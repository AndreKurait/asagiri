# Network qualification on a clean GitHub-hosted Windows x64 runner.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$version = '0.1.16'
$expected = 'd92a4056f1d9fd617ca7a3ab27615e9c750619cfa0b3767c526fc0564f5eae5c'
$asset = "Asagiri-$version-x86_64.msi"
$root = Join-Path $env:RUNNER_TEMP 'asagiri-msi-network-114'
New-Item -ItemType Directory $root | Out-Null
$msi = Join-Path $root $asset
curl.exe --fail --silent --show-error --location --output $msi "https://github.com/AndreKurait/asagiri/releases/download/v$version/$asset"
$digest = (Get-FileHash -Algorithm SHA256 $msi).Hash
if ($digest -ne $expected) { throw "Unexpected published MSI hash $digest" }
Write-Host "Verified $asset SHA-256 $digest"
$install = Start-Process msiexec -ArgumentList "/i `"$msi`" /qn /norestart" -Wait -PassThru
if ($install.ExitCode) { throw "Installer exited $($install.ExitCode)" }
$bundle = Join-Path $env:LOCALAPPDATA Asagiri
$docker = Join-Path $bundle 'bin\docker.exe'
$bridge = $null
$imported = $false
try {
    wsl.exe --import asagiri "$root\distro" "$bundle\asagiri-linux.tar.gz" --version 2
    $imported = $true
    foreach ($name in 'asagiri','asagiri-init') {
        $inside = (wsl.exe -d asagiri -u root -- sha256sum "/usr/local/bin/$name").Split(' ')[0]
        if ($inside -ne (Get-FileHash "$bundle\$name").Hash) { throw "Packaged $name mismatch" }
        Write-Host "Installed guest $name SHA-256 $inside"
    }
    $app = Join-Path $bundle 'asagiri-win.exe'
    Write-Host "Installed Windows launcher SHA-256 $((Get-FileHash $app).Hash)"
    $env:DOCKER_HOST = 'npipe:////./pipe/asagiri-release-test'
    Remove-Item Env:DOCKER_CONTEXT -ErrorAction SilentlyContinue
    $bridge = Start-Process $app -ArgumentList '--background','--distro','asagiri','--pipe','\\.\pipe\asagiri-release-test' -PassThru
    $ready = $false
    for ($i = 0; $i -lt 45; $i++) {
        try { & $docker info --format '{{.ServerVersion}}'; $ready = $true; break } catch { Start-Sleep -Seconds 2 }
    }
    if (!$ready) { throw 'Packaged Windows launcher did not reach the guest engine' }
    $engine = (& $docker version --format '{{json .Server}}') | ConvertFrom-Json
    if ($engine.Components[0].Details.AsagiriVersion -ne $version) {
        throw "Packaged engine did not identify itself as v$version"
    }
    $image = 'alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab'
    & $docker pull $image
    & $docker tag $image 'alpine:3.21'
    wsl.exe -d asagiri -u root -- apk add --no-cache python3
    $sourcePath = (Resolve-Path "$PSScriptRoot/../release-source/tests").Path.Replace('\','/')
    $fixture = (wsl.exe -d asagiri -u root -- wslpath -a $sourcePath).Trim()
    $socket = '/var/lib/asagiri-daemon/.asagiri/run/docker.sock'
    wsl.exe -d asagiri -u root -- test -S $socket
    $tests = 'network-connect-dns','network-bridge-icc','network-bridge-mtu','network-endpoint-metadata','network-ipvlan','network-macvlan'
    foreach ($test in $tests) {
        wsl.exe -d asagiri -u root -- python3 "$fixture/$test.py" $socket 'alpine:3.21'
        Write-Host "PASS $test on the published Windows MSI"
    }
    if (& $docker ps -aq) { throw 'The network fixtures left containers behind' }
    $networks = @(& $docker network ls --format '{{.Name}}' | Sort-Object)
    if (($networks -join ',') -ne 'bridge,host,none') { throw "Unexpected networks after cleanup: $networks" }
    Write-Host 'All disposable containers and networks were removed.'
} finally {
    if ($bridge -and !$bridge.HasExited) { Stop-Process -Id $bridge.Id -Force; $bridge.WaitForExit() }
    if ($imported) { wsl.exe --unregister asagiri }
    $uninstall = Start-Process msiexec -ArgumentList "/x `"$msi`" /qn /norestart" -Wait -PassThru
    if ($uninstall.ExitCode) { throw "Uninstall exited $($uninstall.ExitCode)" }
}
Write-Host "PASS v$version MSI $digest"
