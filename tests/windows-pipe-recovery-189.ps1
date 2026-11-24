# Runs on an ephemeral Windows runner with the Windows and Linux artifacts from this build.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$root = Join-Path $env:RUNNER_TEMP 'drive-watch'
New-Item -ItemType Directory $root | Out-Null
$asset = 'Asagiri-0.1.14-dev.205-x86_64.msi'
gh release download v0.1.14-dev.205 --repo AndreKurait/asagiri --pattern $asset --dir $root
if ((Get-FileHash "$root\$asset").Hash -ne '1592852ad8ae07a8b903592ec868164697ba49a38629e47de55127da646dc8b2') { throw 'Unexpected base MSI' }
$install = Start-Process msiexec -ArgumentList "/i `"$root\$asset`" /qn /norestart" -Wait -PassThru
if ($install.ExitCode) { throw "Installer exited $($install.ExitCode)" }
$bundle = Join-Path $env:LOCALAPPDATA Asagiri
$docker = Join-Path $bundle 'bin\docker.exe'
wsl.exe --import asagiri "$root\distro" "$bundle\asagiri-linux.tar.gz" --version 2
try {
    $upgrade = 'Asagiri-0.1.14-dev.277-x86_64.msi'
    gh release download v0.1.14-dev.277 --repo AndreKurait/asagiri --pattern $upgrade --dir $root
    if ((Get-FileHash "$root\$upgrade").Hash -ne '6e9b3dd16222d3b016fc944931fe0c45b4de0f328ce7668942268baf87ebc07f') { throw 'Unexpected upgrade MSI' }
    $install = Start-Process msiexec -ArgumentList "/i `"$root\$upgrade`" /qn /norestart" -Wait -PassThru
    if ($install.ExitCode) { throw "Upgrade exited $($install.ExitCode)" }
    $expectedEngine = (Get-FileHash "$bundle\asagiri").Hash
    $oldEngine = (wsl.exe -d asagiri -u root -- sha256sum /usr/local/bin/asagiri).Split(' ')[0]
    if ($oldEngine -eq $expectedEngine) { throw 'Expected an older guest engine before upgrade' }
    @{done=$true;linux='asagiri';distro='asagiri'} | ConvertTo-Json | Set-Content -Encoding utf8NoBOM "$bundle\setup.json"
    # Start the launcher with the old guest binary installed and no daemon running.
    wsl.exe -d asagiri -u root -- sed -i '/^command *=/d' /etc/wsl.conf
    wsl.exe --terminate asagiri
    $env:ASAGIRI_WIN_WATCH_BINDS = '1'
    $env:DOCKER_HOST = 'npipe:////./pipe/asagiri-watch-test'
    Remove-Item Env:DOCKER_CONTEXT -ErrorAction SilentlyContinue
    $app = Join-Path $bundle 'asagiri-win.exe'
    $bridge = Start-Process $app -ArgumentList '--background','--distro','asagiri','--pipe','\\.\pipe\asagiri-watch-test' -PassThru
    $ready = $false
    for ($i = 0; $i -lt 45; $i++) {
        try { & $docker info --format '{{.ServerVersion}}'; $ready = $true; break } catch { Start-Sleep -Seconds 2 }
    }
    if (!$ready) { throw 'Windows pipe did not reach daemon' }
    for ($i = 0; $i -lt 60; $i++) {
        $actual = (wsl.exe -d asagiri -u root -- sha256sum /usr/local/bin/asagiri).Split(' ')[0]
        if ($actual -eq $expectedEngine) { break }
        Start-Sleep -Seconds 1
    }
    if ($actual -ne $expectedEngine) { throw "Launcher did not install the new engine: $actual" }
    $probeScript = (Resolve-Path tests/wsl-bridge-process-189.sh).Path.Replace('\','/')
    $probePath = (wsl.exe -d asagiri -u root -- wslpath -a $probeScript).Trim()
    $bridgeHash = (wsl.exe -d asagiri -u root -- sh $probePath).Trim()
    if ($bridgeHash -ne $expectedEngine) { throw "Running guest bridge has the wrong revision: $bridgeHash" }
    Write-Host "Launcher installed engine $actual over $oldEngine and the running bridge uses $bridgeHash"
} finally {
    if ($bridge -and !$bridge.HasExited) { Stop-Process -Id $bridge.Id }
    wsl.exe --unregister asagiri
}
