# One-off #217 qualification on a clean GitHub-hosted Windows x64 runner.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$version = '0.1.16-dev.349'
$expected = 'a9325ea45041f0e91471e7d561e0c52be9a8b00c727f27bbf47318caea7efffd'
$asset = "Asagiri-$version-x86_64.msi"
$root = Join-Path $env:RUNNER_TEMP 'asagiri-msi-217'
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
    $src = (wsl.exe -d asagiri -u root -- wslpath -a ($bundle.Replace('\','/'))).Trim()
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
    $image = 'node:22.23.5-bookworm-slim'
    & $docker pull $image
    # Reuse the downloaded Linux image as a second distro with no Asagiri boot hook.
    $probe = (& $docker create $image node -e "require('http').createServer((q,r)=>r.end('owner-alive')).listen(18080)").Trim()
    try {
        & $docker export -o "$root/netns-peer.tar" $probe
        & $docker start $probe
        $address = (& $docker inspect --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $probe).Trim()
        function Assert-OwnerTraffic {
            for ($i = 0; $i -lt 10; $i++) {
                try {
                    $reply = wsl.exe -d asagiri -u root -- wget -q -T 2 -O - "http://${address}:18080/"
                    if ($reply -ceq 'owner-alive') { return }
                } catch { }
                Start-Sleep -Milliseconds 500
            }
            throw 'First engine container stopped answering over its bridge'
        }
        Assert-OwnerTraffic
        wsl.exe -d asagiri -u root -- apk add --no-cache nftables
        function Get-OwnerFirewall {
            $rules = (wsl.exe -d asagiri -u root -- nft --stateless --handle list table ip asagiri) -join "`n"
            if ($rules -notmatch 'chain ' -or $rules -notmatch '# handle ') { throw 'Missing Asagiri firewall chains or handles' }
            return $rules
        }
        $firewall = Get-OwnerFirewall
        $ifindex = (wsl.exe -d asagiri -u root -- cat /sys/class/net/asagiri0/ifindex).Trim()
        $namespace = (wsl.exe -d asagiri -u root -- readlink /proc/self/ns/net).Trim()
        wsl.exe --import asagiri-netns-peer "$root/netns-peer" "$root/netns-peer.tar" --version 2
        try {
            $peerNamespace = (wsl.exe -d asagiri-netns-peer -u root -- readlink /proc/self/ns/net).Trim()
            if ($peerNamespace -ne $namespace) { throw 'WSL distributions do not share the test network namespace' }
            # Check the expected nonzero exit without suppressing other native failures.
            $result = & {
                $PSNativeCommandUseErrorActionPreference = $false
                $lines = wsl.exe -d asagiri-netns-peer -u root -- env ASAGIRI_AGENT_DIR=/tmp/agent timeout 15 "$src/asagiri-init" 2>&1
                @{ Code = $LASTEXITCODE; Text = ($lines -join "`n") }
            }
            if ($result.Code -ne 1 -or $result.Text -notmatch 'network namespace is already claimed') { throw "Unexpected peer result: $($result.Code) $($result.Text)" }
            Assert-OwnerTraffic
            if ((Get-OwnerFirewall) -cne $firewall) { throw 'Peer startup changed the owner firewall' }
        } finally { wsl.exe --unregister asagiri-netns-peer }
        Assert-OwnerTraffic
        if ((Get-OwnerFirewall) -cne $firewall) { throw 'Peer teardown changed the owner firewall' }
        if ((wsl.exe -d asagiri -u root -- cat /sys/class/net/asagiri0/ifindex).Trim() -ne $ifindex) { throw 'First engine bridge was replaced' }
    } finally { & $docker rm -f $probe }
    Write-Output "PASS owner traffic, bridge ifindex $ifindex, unchanged firewall and peer teardown in $namespace"
} finally {
    if ($bridge -and !$bridge.HasExited) { Stop-Process -Id $bridge.Id -Force; $bridge.WaitForExit() }
    if ($imported) { wsl.exe --unregister asagiri }
    $uninstall = Start-Process msiexec -ArgumentList "/x `"$msi`" /qn /norestart" -Wait -PassThru
    if ($uninstall.ExitCode) { throw "Uninstall exited $($uninstall.ExitCode)" }
}
Write-Host "PASS v$version MSI $digest"
