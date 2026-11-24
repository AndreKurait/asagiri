# Called with a running PR-built engine on its isolated test pipe.
param([string]$Docker, [string]$Root)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$distro = 'Development138'
$image = 'docker:28-cli'
& $Docker pull $image
$id = (& $Docker create $image docker --version).Trim()
try { & $Docker export --output "$Root\development.tar" $id } finally { & $Docker rm $id }
wsl.exe --import $distro "$Root\development" "$Root\development.tar" --version 2
try {
    wsl.exe -d $distro -u root -- adduser -D -u 1001 builder
    wsl.exe -d $distro -u root -- sh -ec 'printf "[user]\ndefault=builder\n" > /etc/wsl.conf'
    wsl.exe -d $distro -u builder -- sh -ec 'echo distro-local > /home/builder/fixture'
    # WSL reads the default user from wsl.conf when the distribution starts.
    wsl.exe --terminate $distro
    $defaultUser = (wsl.exe -d $distro -- id -un).Trim()
    if ($defaultUser -ne 'builder') { throw "Unexpected default distro user $defaultUser" }
    $test = Join-Path $env:RUNNER_TEMP 'windows-X64\wsl-settings-test.exe'
    function Set-Integration([string]$mode) {
        $env:ASAGIRI_TEST_DISTRO_SETTING = $mode
        $output = & $test --exact wslconfig::native_integration::change_development138_setting --ignored --nocapture
        $output | Write-Host
        if (($output -join "`n") -notmatch '1 passed; 0 failed') { throw 'Settings test did not execute' }
    }
    function Read-DistroBind {
        $uid = (wsl.exe -d $distro -u builder -- id -u).Trim()
        if ($uid -ne '1001') { throw "Unexpected client uid $uid" }
        # Capture the effective WSL login groups and permissions on every socket ancestor.
        # These observations survive fixture cleanup if the following Docker command fails.
        wsl.exe -d $distro -u builder -- id
        wsl.exe -d $distro -u root -- stat -c '%a %u:%g %n' /mnt /mnt/wsl /mnt/wsl/asagiri /mnt/wsl/asagiri/sockets /mnt/wsl/asagiri/sockets/Development138 /mnt/wsl/asagiri/sockets/Development138/docker.sock
        $text = wsl.exe -d $distro -u builder -- docker --context asagiri run --rm --network none --mount 'type=bind,src=/home/builder/fixture,dst=/fixture,readonly' $image cat /fixture
        if (($text -join "`n").Trim() -ne 'distro-local') { throw "Wrong distro bind contents: $text" }
    }
    Set-Integration on
    Read-DistroBind
    wsl.exe --shutdown
    $restored = $false
    for ($i = 0; $i -lt 35; $i++) {
        try { & $Docker info --format '{{.ServerVersion}}'; Read-DistroBind; $restored = $true; break } catch { Start-Sleep -Seconds 2 }
    }
    if (!$restored) { throw 'Ordinary-user distro bind did not recover after WSL shutdown' }
    # End the settings process after recording the disable, before cleanup.
    # A new process runs the normal background reconnect path.
    Set-Integration interrupt-off
    Set-Integration reconnect
    wsl.exe -d $distro -u builder -- sh -ec 'test ! -L /var/run/docker.sock && test ! -S /mnt/wsl/asagiri/sockets/Development138/docker.sock && ! grep -Fq " /mnt/wsl/asagiri/roots/Development138 " /proc/mounts'
    Set-Integration on
    Read-DistroBind
    # Check independent selections through Settings using two real WSL distros.
    $second = 'Build138'
    wsl.exe --import $second "$Root\build138" "$Root\development.tar" --version 2
    try {
        Set-Integration second-on
        $secondSocket = '/mnt/wsl/asagiri/sockets/Build138/docker.sock'
        $before = (wsl.exe -d $second -u root -- stat -c '%i:%g:%a' $secondSocket).Trim()
        Set-Integration off
        wsl.exe -d $distro -u builder -- sh -ec 'test ! -L /var/run/docker.sock && test ! -S /mnt/wsl/asagiri/sockets/Development138/docker.sock'
        $after = (wsl.exe -d $second -u root -- stat -c '%i:%g:%a' $secondSocket).Trim()
        if ($before -ne $after) { throw "Second listener changed from $before to $after" }
        wsl.exe -d $second -u root -- docker -H "unix://$secondSocket" info --format '{{.ServerVersion}}'
        Set-Integration second-off
    } finally {
        wsl.exe --unregister $second
    }
} finally {
    wsl.exe --unregister $distro
}
