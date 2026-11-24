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
$linux = Join-Path $env:RUNNER_TEMP linux-X64
if ((Get-Content "$linux\SOURCE_COMMIT").Trim() -ne 'c501a37d18d1c6d5c5dc50e8531a16b66eaf26a8') { throw 'Linux binary revision mismatch' }
wsl.exe --import asagiri "$root\distro" "$bundle\asagiri-linux.tar.gz" --version 2
try {
    $expectedEngine = (Get-FileHash "$linux\asagiri").Hash
    $oldEngine = (wsl.exe -d asagiri -u root -- sha256sum /usr/local/bin/asagiri).Split(' ')[0]
    if ($oldEngine -eq $expectedEngine) { throw 'Expected an older guest engine before upgrade' }
    foreach ($name in 'asagiri','asagiri-init') { Copy-Item "$linux\$name" "$bundle\$name" -Force }
    @{done=$true;linux='asagiri';distro='asagiri'} | ConvertTo-Json | Set-Content -Encoding utf8NoBOM "$bundle\setup.json"
    # Start the launcher with the old guest binary installed and no daemon running.
    wsl.exe -d asagiri -u root -- sed -i '/^command *=/d' /etc/wsl.conf
    wsl.exe --terminate asagiri
    $env:ASAGIRI_WIN_WATCH_BINDS = '1'
    $env:DOCKER_HOST = 'npipe:////./pipe/asagiri-watch-test'
    Remove-Item Env:DOCKER_CONTEXT -ErrorAction SilentlyContinue
    $app = Join-Path $env:RUNNER_TEMP 'windows-X64\asagiri-win.exe'
    $bridge = Start-Process $app -ArgumentList '--background','--distro','asagiri','--pipe','\\.\pipe\asagiri-watch-test' -PassThru
    $ready = $false
    for ($i = 0; $i -lt 45; $i++) {
        try { & $docker info --format '{{.ServerVersion}}'; $ready = $true; break } catch { Start-Sleep -Seconds 2 }
    }
    if (!$ready) { throw 'Windows pipe did not reach daemon' }
    $actual = (wsl.exe -d asagiri -u root -- sha256sum /usr/local/bin/asagiri).Split(' ')[0]
    if ($actual -ne $expectedEngine) { throw "Launcher did not install the new engine: $actual" }
    $bridgeHash = (wsl.exe -d asagiri -u root -- sh -ec 'pid=$(pgrep -f "^/usr/local/bin/asagiri bridge .*docker.sock" | head -1); test -n "$pid"; sha256sum /proc/$pid/exe').Split(' ')[0]
    if ($bridgeHash -ne $expectedEngine) { throw "Running guest bridge has the wrong revision: $bridgeHash" }
    Write-Host "Launcher installed engine $actual over $oldEngine and the running bridge uses $bridgeHash"
    Stop-Process -Id $bridge.Id
    $bridge.WaitForExit()
    $socketDir = '/var/lib/asagiri-daemon/.asagiri/run'
    $probe = "$socketDir/recovery189.sock"
    # Start with a real listener, then remove only the fixture's alias.
    wsl.exe -d asagiri -u root -- ln -s "$socketDir/docker.sock" $probe
    $bridge = Start-Process $app -ArgumentList '--background','--distro','asagiri','--pipe','\\.\pipe\asagiri-watch-test','--socket',$probe -PassThru
    $ready = $false
    for ($i = 0; $i -lt 30; $i++) {
        try { & $docker info --format '{{.ServerVersion}}'; $ready = $true; break } catch { Start-Sleep -Milliseconds 500 }
    }
    if (!$ready) { throw 'The alternate guest socket did not reach the real engine' }
    wsl.exe -d asagiri -u root -- rm $probe
    $failure = ''
    try { & $docker info 2>&1 | Out-String | Write-Host } catch { $failure = $_.ToString() }
    if (!$failure) { throw 'Expected a failed request before repair' }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    wsl.exe -d asagiri -u root -- ln -s "$socketDir/docker.sock" $probe
    $version = (& $docker info --format '{{.ServerVersion}}').Trim()
    $clock.Stop()
    if (!$version -or $clock.Elapsed.TotalSeconds -ge 5) { throw "Recovery exceeded initial backoff: $version $($clock.Elapsed)" }
    Write-Host "Repaired guest socket served Docker $version within $($clock.ElapsedMilliseconds) ms through the same launcher"
    # Preserve the old socket inode when the real daemon unlinks and rebinds its path.
    wsl.exe -d asagiri -u root -- ln "$socketDir/docker.sock" "$socketDir/stale189.sock"
    wsl.exe -d asagiri -u root -- /usr/local/bin/asagiri-start --restart
    $old = (wsl.exe -d asagiri -u root -- stat -c '%d:%i' "$socketDir/stale189.sock").Trim()
    $new = (wsl.exe -d asagiri -u root -- stat -c '%d:%i' "$socketDir/docker.sock").Trim()
    if ($old -eq $new) { throw 'Daemon restart did not replace the socket inode' }
    wsl.exe -d asagiri -u root -- ln -sfn "$socketDir/stale189.sock" $probe
    $clock.Restart()
    $PSNativeCommandUseErrorActionPreference = $false
    $reply = (& $docker info 2>&1 | Out-String)
    $exitCode = $LASTEXITCODE
    $PSNativeCommandUseErrorActionPreference = $true
    $clock.Stop()
    if (!$exitCode -or $reply -notmatch 'socket refused connections after startup' -or $clock.Elapsed.TotalSeconds -ge 10) {
        throw "Unexpected stale socket response in $($clock.ElapsedMilliseconds) ms: $reply"
    }
    Write-Host "Stale guest socket returned the startup refusal in $($clock.ElapsedMilliseconds) ms"
} finally {
    if ($bridge -and !$bridge.HasExited) { Stop-Process -Id $bridge.Id }
    wsl.exe --unregister asagiri
}
