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
if ((Get-Content "$linux\SOURCE_COMMIT").Trim() -ne (git rev-parse HEAD)) { throw 'Linux binary revision mismatch' }
wsl.exe --import asagiri "$root\distro" "$bundle\asagiri-linux.tar.gz" --version 2
try {
    $src = (wsl.exe -d asagiri -u root -- wslpath -a ($linux.Replace('\','/'))).Trim()
    foreach ($name in 'asagiri','asagiri-init') {
        # Install through a temporary pathname since the base daemon may already be running.
        wsl.exe -d asagiri -u root -- cp "$src/$name" "/usr/local/bin/$name.new"
        wsl.exe -d asagiri -u root -- chmod 755 "/usr/local/bin/$name.new"
        wsl.exe -d asagiri -u root -- mv "/usr/local/bin/$name.new" "/usr/local/bin/$name"
        $digest = (wsl.exe -d asagiri -u root -- sha256sum "/usr/local/bin/$name").Split(' ')[0]
        if ($digest -ne (Get-FileHash "$linux\$name").Hash) { throw "Wrong installed $name" }
    }
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
    try {
    $directory = Join-Path $env:USERPROFILE 'asagiri-watch-test'
    if ([IO.Path]::GetPathRoot($directory) -ne 'C:\') { throw 'Fixture requires C drive' }
    New-Item -ItemType Directory $directory | Out-Null
    $script = @'
const fs = require('node:fs');
let nestedWatcher;
let watchedGeneration;
function watchIdentity(stage) {
  try {
    const pathInode = fs.statSync('/watched/renamed', {bigint:true}).ino.toString(16);
    const marks = fs.readdirSync('/proc/self/fdinfo').flatMap(fd => {
      try { return fs.readFileSync('/proc/self/fdinfo/'+fd, 'utf8').split('\n').filter(line => line.startsWith('inotify wd:')); }
      catch (error) { if (error.code === 'ENOENT') return []; throw error; }
    });
    console.log(JSON.stringify({stage, pathInodeHex:pathInode, marks}));
  } catch (error) { console.log('watch identity unavailable: '+error.message); }
}
process.on('SIGUSR2', () => watchIdentity('timeout'));
function snapshot() {
  const state = {};
  for (const entry of fs.readdirSync('/watched', {withFileTypes:true})) {
    if (entry.isFile()) state[entry.name] = fs.readFileSync('/watched/'+entry.name,'utf8');
    if (entry.isDirectory() && entry.name === 'renamed') {
      const dir = '/watched/renamed';
      const generation = fs.existsSync(dir+'/replacement') ? 'replacement' : 'original';
      if (generation === watchedGeneration) continue;
      nestedWatcher?.close();
      watchedGeneration = generation;
      nestedWatcher = fs.watch(dir, () => {
        try { console.log(generation+' child: '+fs.readFileSync(dir+'/child','utf8')); }
        catch (error) { if (error.code !== 'ENOENT') throw error; }
      });
      console.log(generation+' READY');
      watchIdentity(generation);
    }
  }
  console.log(JSON.stringify(state));
}
fs.watch('/watched', snapshot);
console.log('READY');
'@
    $image = 'node:22.23.5-bookworm-slim'
    & $docker pull $image
    $socketDir = '/var/lib/asagiri-daemon/.asagiri/run'
    $hostSocket = (wsl.exe -d asagiri -u root -- stat -Lc '%d:%i' "$socketDir/docker.sock").Trim()
    $clientSocket = (wsl.exe -d asagiri -u root -- stat -Lc '%d:%i' "$socketDir/docker.container.sock").Trim()
    if ($hostSocket -eq $clientSocket) { throw 'Expected two distinct engine sockets' }
    $mountedSocket = (& $docker run --rm --network none --mount 'type=bind,source=/var/run/docker.sock,target=/engine.sock,readonly' $image stat -Lc '%d:%i' /engine.sock).Trim()
    if ($mountedSocket -ne $clientSocket) { throw "Socket bind resolved to $mountedSocket, expected $clientSocket" }
    Write-Host "Container socket bind matches the client listener ($mountedSocket)"
    & $docker run -d --name watch-test --mount "type=bind,source=$directory,target=/watched,readonly" $image node -e $script
    function Await-Line([string]$expected, [int]$offset) {
        for ($i = 0; $i -lt 40; $i++) {
            $lines = @(& $docker logs watch-test)
            if (($lines | Select-Object -Skip $offset) -ccontains $expected) { return }
            Start-Sleep -Milliseconds 500
        }
        if ($expected -like '*child:*') {
            & $docker kill --signal=USR2 watch-test | Out-Null
            Start-Sleep -Seconds 1
            & $docker logs watch-test | Write-Host
        }
        throw "Watcher missed $expected after line $offset. Observed $lines"
    }
    Await-Line 'READY' 0
    # Allow the Windows coordinator's five-second inventory refresh to register this bind.
    Start-Sleep -Seconds 7
    $offset = @(& $docker logs watch-test).Count
    [IO.File]::WriteAllText("$directory\a", 'one'); Await-Line '{"a":"one"}' $offset
    $offset = @(& $docker logs watch-test).Count
    [IO.File]::WriteAllText("$directory\a", 'two'); Await-Line '{"a":"two"}' $offset
    $offset = @(& $docker logs watch-test).Count
    Move-Item "$directory\a" "$directory\b"; Await-Line '{"b":"two"}' $offset
    $offset = @(& $docker logs watch-test).Count
    Remove-Item "$directory\b"; Await-Line '{}' $offset
    New-Item -ItemType Directory "$directory\original" | Out-Null
    [IO.File]::WriteAllText("$directory\original\child", 'before')
    $offset = @(& $docker logs watch-test).Count
    Move-Item "$directory\original" "$directory\renamed"
    Await-Line 'original READY' $offset
    $offset = @(& $docker logs watch-test).Count
    [IO.File]::WriteAllText("$directory\renamed\child", 'after')
    Await-Line 'original child: after' $offset
    New-Item -ItemType Directory "$directory\staged" | Out-Null
    [IO.File]::WriteAllText("$directory\staged\replacement", '')
    [IO.File]::WriteAllText("$directory\staged\child", 'before replacement edit')
    $offset = @(& $docker logs watch-test).Count
    [IO.Directory]::Move("$directory\renamed", "$directory\retired")
    [IO.Directory]::Move("$directory\staged", "$directory\renamed")
    Await-Line 'replacement READY' $offset
    $offset = @(& $docker logs watch-test).Count
    [IO.File]::WriteAllText("$directory\renamed\child", 'later edit')
    Await-Line 'replacement child: later edit' $offset
    & $docker logs watch-test
    Stop-Process -Id $bridge.Id -Force
    $bridge.WaitForExit()
    $env:ASAGIRI_WIN_WATCH_BINDS = '0'
    $bridge = Start-Process $app -ArgumentList '--background','--distro','asagiri','--pipe','\\.\pipe\asagiri-watch-test' -PassThru
    $ready = $false
    for ($i = 0; $i -lt 45; $i++) {
        try { & $docker info --format '{{.ServerVersion}}'; $ready = $true; break } catch { Start-Sleep -Seconds 2 }
    }
    if (!$ready) { throw 'Opt-out bridge did not start' }
    $offset = @(& $docker logs watch-test).Count
    [IO.File]::WriteAllText("$directory\disabled", 'visible without forwarding')
    $visible = & $docker exec watch-test cat /watched/disabled
    if ($visible -ne 'visible without forwarding') { throw 'Opt-out bind did not expose the file' }
    Start-Sleep -Seconds 12
    $newLines = @(& $docker logs watch-test) | Select-Object -Skip $offset
    if ($newLines -match '"disabled":"visible without forwarding"') { throw 'Opt-out watcher forwarded a host file creation' }
    } finally {
        & "$PSScriptRoot/windows-wsl-integration.ps1" -Docker $docker -Root $root
    }
} finally {
    if ($bridge -and !$bridge.HasExited) { Stop-Process -Id $bridge.Id }
    wsl.exe --unregister asagiri
}
