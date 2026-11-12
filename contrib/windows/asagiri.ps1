# Asagiri on Windows: installs the Linux engine into a WSL2 distro, and
# asagiri-win.exe, which serves the Docker API on \\.\pipe\asagiri for the Windows
# docker CLI (as the `asagiri` context) and carries it into WSL. It brings the
# docker CLI, Compose and buildx too, as Asagiri.app does, so no other Docker
# is needed. Per user: no admin, no service, no change to WSL's or Windows'
# network settings.
#   powershell -ExecutionPolicy Bypass -File asagiri.ps1 [-Distro Ubuntu] [-Bundle C:\path\to\asagiri-windows-x86_64 (or -aarch64)] [-Default]
#   powershell -ExecutionPolicy Bypass -File asagiri.ps1 -Uninstall [-Distro Ubuntu]
#   powershell -ExecutionPolicy Bypass -File asagiri.ps1 -Update -Msi <staged .msi> [-WaitFor <pid>]
# The MSI (installer/windows) lays the same files in %LOCALAPPDATA%\Asagiri and
# runs `-Uninstall -IfSetUp` when removed: nothing happens unless first-run
# setup completed, and the distro setup chose is taken from setup.json.
# asagiri-win.exe runs `-Update` (update.rs) when it quits for an update it
# downloaded: once that process is gone the staged MSI upgrades Asagiri
# silently and the new Asagiri opens.
# Asagiri becomes docker's default engine (the current context, and the builder
# `docker build` uses) unless Docker Desktop is installed; -Default makes it
# the default anyway.
param(
  [string]$Distro = "Ubuntu",
  [string]$Bundle = $PSScriptRoot,
  [switch]$Default,
  [switch]$Uninstall,
  [switch]$IfSetUp,
  [switch]$Update,
  [string]$Msi,
  [int]$WaitFor
)
$ErrorActionPreference = "Stop"

# -Update: waits for the asagiri-win that downloaded the MSI to quit (its files
# are in use until then), upgrades silently (per user, no prompt; 3010 is
# "done, restart Windows later"), clears the stage when that went through,
# and opens the new Asagiri. Each step goes to helper.log beside the MSI, which
# stays, with msiexec's install.log, when the upgrade failed.
if ($Update) {
  if (-not $Msi) { throw "-Update needs -Msi" }
  $stage = Split-Path $Msi -Parent
  $helperLog = Join-Path $stage "helper.log"
  function Say($m) { Add-Content -LiteralPath $helperLog -Value "$(Get-Date -Format s) $m" }
  try {
    Say "waiting for asagiri-win $WaitFor"
    while ($WaitFor -and (Get-Process -Id $WaitFor -ErrorAction SilentlyContinue)) { Start-Sleep -Milliseconds 200 }
    Say "installing $Msi"
    $p = Start-Process -FilePath (Join-Path $env:SystemRoot "System32\msiexec.exe") -ArgumentList "/i", "`"$Msi`"", "/qn", "/norestart", "/L*V", "`"$(Join-Path $stage 'install.log')`"" -Wait -PassThru
    Say "msiexec exit $($p.ExitCode)"
    if ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
    Start-Process -FilePath (Join-Path $env:LOCALAPPDATA "Asagiri\asagiri-win.exe")
  } catch { Say "failed: $_" }
  return
}

$setupJson = Join-Path $env:LOCALAPPDATA "Asagiri\setup.json"
if ($IfSetUp -and -not (Test-Path $setupJson)) { return }
if (-not $PSBoundParameters.ContainsKey("Distro") -and (Test-Path $setupJson)) {
  $saved = (Get-Content $setupJson -Raw | ConvertFrom-Json).distro
  if ($saved) { $Distro = $saved }
}
if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) { throw "WSL is not installed: run 'wsl --install' first" }
$dir = Join-Path $env:LOCALAPPDATA "Asagiri"
$exe = Join-Path $dir "asagiri-win.exe"
$bin = Join-Path $dir "bin"
$plugins = Join-Path $bin "cli-plugins"
$link = Join-Path ([Environment]::GetFolderPath("Startup")) "Asagiri.lnk"
$dockerConfig = Join-Path $env:USERPROFILE ".docker\config.json"
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# Asagiri's bin on this user's PATH: last, so a docker CLI already installed
# (Docker Desktop's is on the system PATH, which comes first) keeps winning.
# Through the registry, not [Environment]::SetEnvironmentVariable, which
# would write REG_SZ and turn %USERPROFILE% and the like in the user's PATH
# into fixed paths; the value keeps its kind (REG_EXPAND_SZ) and its text.
function Set-AsagiriPath([bool]$On) {
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
  try {
    $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    $raw = ""
    if ($key.GetValueNames() -contains "Path") {
      $kind = $key.GetValueKind("Path")
      $raw = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    }
    $parts = @(($raw -split ";") | Where-Object { $_ -and $_ -ne $bin -and ([Environment]::ExpandEnvironmentVariables($_) -ne $bin) })
    if ($On) { $parts += $bin }
    if ($parts.Count) { $key.SetValue("Path", ($parts -join ";"), $kind) } else { $key.DeleteValue("Path", $false) }
  } finally { $key.Close() }
  # What SetEnvironmentVariable would have done: tell Explorer and the rest
  # that the environment changed, so new terminals see the new PATH.
  if (-not ("Asagiri.Env" -as [type])) {
    Add-Type -Namespace Asagiri -Name Env -MemberDefinition '[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);'
  }
  $r = [UIntPtr]::Zero
  [Asagiri.Env]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$r) | Out-Null
  $env:Path = (@(($env:Path -split ";") | Where-Object { $_ -and $_ -ne $bin }) + $(if ($On) { $bin } else { @() })) -join ";"
}

# Compose and buildx through the docker config's cliPluginsExtraDirs. The
# config also holds registry logins: it is merged, never replaced, and
# backed up once before Asagiri first changes it.
function Set-AsagiriPlugins([bool]$On) {
  $cfg = if (Test-Path $dockerConfig) { Get-Content $dockerConfig -Raw | ConvertFrom-Json } else { [pscustomobject]@{} }
  $dirs = @($cfg.cliPluginsExtraDirs | Where-Object { $_ -and $_ -ne $plugins })
  if ($On) { $dirs += $plugins }
  if ($null -eq $cfg.cliPluginsExtraDirs -and -not $On) { return }
  if ((Test-Path $dockerConfig) -and -not (Test-Path "$dockerConfig.before-asagiri")) { Copy-Item $dockerConfig "$dockerConfig.before-asagiri" }
  if ($dirs.Count) { $cfg | Add-Member -NotePropertyName cliPluginsExtraDirs -NotePropertyValue $dirs -Force }
  else { $cfg.PSObject.Properties.Remove("cliPluginsExtraDirs") }
  New-Item -ItemType Directory -Force (Split-Path $dockerConfig) | Out-Null
  # UTF-8 without a BOM: Set-Content -Encoding utf8 writes one in Windows
  # PowerShell, and docker then rejects the whole file, logins included.
  [IO.File]::WriteAllText($dockerConfig, ($cfg | ConvertTo-Json -Depth 32), (New-Object Text.UTF8Encoding $false))
  $dockerExe = Join-Path $bin "docker.exe"
  if (-not (Test-Path $dockerExe)) { $dockerExe = (Get-Command docker.exe -ErrorAction SilentlyContinue).Source }
  if ($dockerExe) {
    $prev = $ErrorActionPreference; $ErrorActionPreference = "Continue"
    $out = (& $dockerExe context ls 2>&1 | ForEach-Object { "$_" }) -join "`n"
    $ErrorActionPreference = $prev
    if ($out -match "Error parsing config") { throw "docker cannot read $dockerConfig after Asagiri changed it (the backup is $dockerConfig.before-asagiri): $out" }
  }
}

# A WSL distro's folder (the registry has what `wsl --list` prints as UTF-16).
function Get-WslBasePath([string]$Name) {
  foreach ($k in Get-ChildItem "HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss" -ErrorAction SilentlyContinue) {
    $p = Get-ItemProperty $k.PSPath -ErrorAction SilentlyContinue
    if ($p.DistributionName -eq $Name) { return ([string]$p.BasePath) -replace '^\\\\\?\\', '' }
  }
}

Get-Process asagiri-win -ErrorAction SilentlyContinue | Stop-Process -Force
if ($Uninstall) {
  if ($IfSetUp) {
    # From the MSI: %LOCALAPPDATA%\Asagiri is about to go, so the log lives in %TEMP%.
    Start-Transcript -Path (Join-Path $env:TEMP "asagiri-uninstall.log") -Force | Out-Null
    "PowerShell $PSHOME ($([IntPtr]::Size * 8)-bit), elevated: $elevated, interactive: $([Environment]::UserInteractive), distro: $Distro"
    if ($elevated) {
      Write-Host "Asagiri: not undoing setup from an elevated session (it would act on the administrator's WSL and docker settings, not yours): uninstall from a non-elevated session"
      Stop-Transcript | Out-Null
      exit 1
    }
  }
  # Each step on its own: one that fails (a docker CLI that is gone, WSL not
  # answering) must not keep the next from running.
  $ErrorActionPreference = "Continue"
  $problems = @()
  function Step([string]$What, [scriptblock]$Do) {
    try { & $Do; Write-Host "- $What" } catch { Write-Host "- $What : FAILED: $_"; $script:problems += $What }
  }
  # Uninstalling stops everything Asagiri runs: asagiri-win is gone (above); any
  # asagiri or docker command still running from Asagiri's folder (`asagiri share`, a
  # docker compose up) goes too, before the engine in WSL stops (below).
  Step "Asagiri commands still running stopped" {
    Get-Process asagiri, docker, docker-compose, docker-buildx -ErrorAction SilentlyContinue |
      Where-Object { $_.Path -and $_.Path.StartsWith($dir, [StringComparison]::OrdinalIgnoreCase) } |
      Stop-Process -Force
  }
  # Before the builder goes: docker build's own builder back as it was
  # before Asagiri changed it (crates/asagiri/src/buildwith.rs), from asagiri-win.
  Step "docker build given back its own builder" {
    if (Test-Path $exe) { $p = Start-Process $exe -ArgumentList "--uninstall-builds" -Wait -PassThru; if ($p.ExitCode -ne 0) { throw "asagiri-win exited $($p.ExitCode)" } }
  }
  # The cluster's `asagiri` kubectl context out of ~\.kube\config, the rest
  # of the file kept (crates/asagiri/src/kubeconfig.rs), from asagiri-win.
  Step "kubectl context removed" {
    if (Test-Path $exe) { $p = Start-Process $exe -ArgumentList "--uninstall-kubeconfig" -Wait -PassThru; if ($p.ExitCode -ne 0) { throw "asagiri-win exited $($p.ExitCode)" } }
  }
  Step "docker context and builder removed" {
    $docker = Join-Path $bin "docker.exe"
    if (-not (Test-Path $docker)) { $docker = (Get-Command docker.exe -ErrorAction SilentlyContinue).Source }
    if ($docker) {
      if ((& $docker context show 2>$null) -eq "asagiri") { & $docker context use default 2>$null | Out-Null }
      & $docker buildx rm asagiri-build 2>$null | Out-Null
      & $docker context rm -f asagiri 2>$null | Out-Null
    }
    if ([Environment]::GetEnvironmentVariable("BUILDX_BUILDER", "User") -eq "asagiri-build") { [Environment]::SetEnvironmentVariable("BUILDX_BUILDER", $null, "User") }
  }
  Step "Compose and buildx taken out of $dockerConfig" { Set-AsagiriPlugins $false }
  Step "$bin taken off PATH" { Set-AsagiriPath $false }
  # Both ways it was ever set: the Startup-folder shortcut, and the Run
  # value setup's "Start Asagiri when I sign in" writes (setup.rs).
  Step "start at sign-in removed" {
    Remove-Item $link -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "Asagiri" -ErrorAction SilentlyContinue
  }
  # The engine in WSL. A distro setup imported under %LOCALAPPDATA%\Asagiri\wsl
  # (Asagiri Linux as `asagiri`, or Ubuntu when `wsl --install` was not available)
  # keeps its disk, with the images and containers, in the folder the MSI
  # deletes next: it is unregistered here, so WSL does not keep a distro
  # whose disk is gone. A distro of the user's own only loses the engine.
  $base = Get-WslBasePath $Distro
  $asagiriOwned = $base -and $base.TrimEnd("\").StartsWith($dir.TrimEnd("\") + "\", [StringComparison]::OrdinalIgnoreCase)
  if ($Distro -eq "asagiri" -or $asagiriOwned) {
    Step "WSL distro '$Distro' stopped" { wsl.exe --terminate $Distro 2>$null | Out-Null }
    if ($base) {
      Write-Host "The WSL distro '$Distro' is Asagiri's (its disk is $base\ext4.vhdx): it is unregistered, and the images, containers and volumes on that disk are deleted with it."
      Step "WSL distro '$Distro' unregistered" { wsl.exe --unregister $Distro 2>$null | Out-Null; if ($LASTEXITCODE -ne 0) { throw "wsl --unregister exited $LASTEXITCODE" } }
    } else {
      Write-Host "The WSL distro '$Distro' is not registered (already gone)."
    }
  } else {
    Step "engine removed from $Distro (systemd unit and /usr/local/bin/asagiri*)" {
      wsl.exe -d $Distro -u root sh -c "systemctl disable --now asagiri 2>/dev/null; rm -f /etc/systemd/system/asagiri.service /usr/local/bin/asagiri /usr/local/bin/asagiri-init"
      if ($LASTEXITCODE -ne 0) { throw "wsl exited $LASTEXITCODE" }
    }
    Write-Host "Its images, containers and volumes stay in /var/lib/asagiri and /var/lib/asagiri-daemon in $Distro."
  }
  Step "$dir removed" { Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue }
  # Last, since it asks for approval (Windows shows a UAC prompt): the DNS
  # rule for <name>.asagiri.local, if it was turned on. Declining it, or a
  # session with no desktop to ask on, leaves only this rule behind.
  $nrpt = Get-DnsClientNrptRule -ErrorAction SilentlyContinue | Where-Object { $_.Namespace -contains ".asagiri.local" }
  if ($nrpt) {
    if ([Environment]::UserInteractive) {
      Step "DNS rule for *.asagiri.local removed (asked for approval)" {
        $p = Start-Process "$PSHOME\powershell.exe" -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList "-NoProfile", "-Command", "Get-DnsClientNrptRule | Where-Object { `$_.Namespace -contains '.asagiri.local' } | Remove-DnsClientNrptRule -Force; Clear-DnsClientCache"
        if ($p.ExitCode -ne 0) { throw "exit code $($p.ExitCode)" }
      }
    } else {
      Write-Host "- DNS rule for *.asagiri.local left in place (no desktop to ask for approval on)"
    }
    if (Get-DnsClientNrptRule -ErrorAction SilentlyContinue | Where-Object { $_.Namespace -contains ".asagiri.local" }) {
      Write-Host "  To remove it, in an elevated PowerShell:  Get-DnsClientNrptRule | Where-Object { `$_.Namespace -contains '.asagiri.local' } | Remove-DnsClientNrptRule -Force"
    }
  }
  if ($problems.Count) { Write-Host "Asagiri is removed, with $($problems.Count) step(s) that failed (above)." } else { Write-Host "Asagiri is removed." }
  if ($IfSetUp) { Stop-Transcript | Out-Null }
  if ($problems.Count) { exit 1 }
  return
}

$wslPath = (wsl.exe -d $Distro wslpath -a ($Bundle -replace '\\', '/')).Trim()
wsl.exe -d $Distro -u root sh "$wslPath/install.sh"
if ($LASTEXITCODE -ne 0) { throw "install.sh failed in $Distro" }

New-Item -ItemType Directory -Force $dir, $plugins | Out-Null
Copy-Item (Join-Path $Bundle "asagiri-win.exe") $exe -Force
Copy-Item (Join-Path $Bundle "bin\docker.exe"), (Join-Path $Bundle "bin\asagiri.exe") $bin -Force
Copy-Item (Join-Path $Bundle "bin\cli-plugins\*.exe") $plugins -Force
Set-AsagiriPath $true
Set-AsagiriPlugins $true

# Started at login, for this user only, in the tray.
$shell = New-Object -ComObject WScript.Shell
$s = $shell.CreateShortcut($link)
$s.TargetPath = $exe
$s.Arguments = "--background --distro $Distro"
$s.WorkingDirectory = $dir
$s.Save()
Start-Process $exe -ArgumentList "--distro", $Distro

$docker = (Get-Command docker.exe).Source
& $docker context rm -f asagiri 2>$null | Out-Null
& $docker context create asagiri --description "Asagiri (WSL2)" --docker "host=npipe:////./pipe/asagiri" | Out-Null
# docker build: BuildKit in a Asagiri container, loading what it builds into
# Asagiri (asagiri-win makes it too, if this finds the engine not up yet).
& $docker buildx inspect asagiri-build 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) { & $docker buildx create --name asagiri-build --driver docker-container --driver-opt default-load=true asagiri 2>$null | Out-Null }

# The default engine. `docker build` picks its builder by context name:
# asagiri-win makes the `asagiri` one a copy of Asagiri's, and which builder plain
# `docker build` uses beside Docker Desktop is asked in its window
# (crates/asagiri/src/buildwith.rs).
$desktop = Test-Path (Join-Path $env:ProgramFiles "Docker\Docker\Docker Desktop.exe")
if ($Default -or -not $desktop) {
  & $docker context use asagiri | Out-Null
  Write-Host "Asagiri is docker's default engine: docker run --rm alpine echo hello"
} else {
  Write-Host "Docker Desktop is installed and stays the default: docker --context asagiri run --rm alpine echo hello"
  Write-Host "(to make Asagiri the default, run asagiri.ps1 -Default; docker buildx build --builder asagiri-build builds on Asagiri)"
}
Write-Host "(docker: $docker; open a new terminal for PATH to apply)"
