# Setup step 4, run by asagiri-win (crates/asagiri-win/src/setup.rs): the docker
# CLI, Compose, buildx and the `asagiri` command into %LOCALAPPDATA%\Asagiri\bin,
# the `asagiri` context and Asagiri's builder, and, if asked, Asagiri as docker's
# default. Prints "TASK <n>" as each task finishes, for the setup screen.
# Per user, no admin; the same as asagiri.ps1 does. Installed beside
# asagiri-win.exe, which checks it against the SHA-256 it was built with and runs
#   powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File setup-tools.ps1 -Bundle <folder> [-Default]
param([string]$Bundle, [string]$Pipe = "npipe:////./pipe/asagiri", [switch]$Default)
$ErrorActionPreference = "Stop"
# Runs docker and returns what it printed (both streams), leaving its exit
# code in $LASTEXITCODE. Needed because, with asagiri-win reading stderr and
# $ErrorActionPreference = Stop, Windows PowerShell turns anything a native
# command writes to stderr (docker's "Successfully created context", a
# buildx inspect that rightly finds nothing yet) into a terminating error.
function Docker {
  $prev = $ErrorActionPreference; $ErrorActionPreference = "Continue"
  try { $out = (& $docker @args 2>&1 | ForEach-Object { "$_" }) -join "`n" } finally { $ErrorActionPreference = $prev }
  $out
}
# A failure anywhere ends as one "ERROR <why>" line on stdout, which is
# what asagiri-win shows; without it, the first stderr line would be shown.
trap {
  # @(): one line left would be a string, and [0] its first character.
  $why = @($_.Exception.Message -split "`r?`n" | Where-Object { $_.Trim() })[0]
  "ERROR $why"
  exit 1
}
$dir = Join-Path $env:LOCALAPPDATA "Asagiri"
$bin = Join-Path $dir "bin"
$plugins = Join-Path $bin "cli-plugins"
$dockerConfig = Join-Path $env:USERPROFILE ".docker\config.json"
New-Item -ItemType Directory -Force $plugins | Out-Null
# The MSI installs the bundle straight into %LOCALAPPDATA%\Asagiri: nothing to copy then.
$inPlace = (Resolve-Path $Bundle).Path.TrimEnd('\') -ieq $dir.TrimEnd('\')

if (-not $inPlace) { Copy-Item (Join-Path $Bundle "bin\docker.exe"), (Join-Path $Bundle "bin\asagiri.exe") $bin -Force }
# Asagiri's bin last on this user's PATH: a docker CLI already installed keeps
# winning. Through the registry so the value keeps its kind (REG_EXPAND_SZ)
# and its %USERPROFILE%-style entries; SetEnvironmentVariable would flatten
# them into fixed paths. Then the broadcast it would have sent, so new
# terminals see the change.
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
try {
  $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
  $raw = ""
  if ($key.GetValueNames() -contains "Path") {
    $kind = $key.GetValueKind("Path")
    $raw = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
  }
  $parts = @(($raw -split ";") | Where-Object { $_ -and $_ -ne $bin -and ([Environment]::ExpandEnvironmentVariables($_) -ne $bin) }) + $bin
  $key.SetValue("Path", ($parts -join ";"), $kind)
} finally { $key.Close() }
Add-Type -Namespace Asagiri -Name Env -MemberDefinition '[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);'
$r = [UIntPtr]::Zero
[Asagiri.Env]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$r) | Out-Null
"TASK 0"

if (-not $inPlace) { Copy-Item (Join-Path $Bundle "bin\cli-plugins\docker-compose.exe") $plugins -Force }
# Plugins through cliPluginsExtraDirs; the config holds registry logins, so
# it is merged, and backed up once before Asagiri first changes it.
$cfg = if (Test-Path $dockerConfig) { Get-Content $dockerConfig -Raw | ConvertFrom-Json } else { [pscustomobject]@{} }
$dirs = @(@($cfg.cliPluginsExtraDirs) | Where-Object { $_ -and $_ -ne $plugins }) + $plugins
if ((Test-Path $dockerConfig) -and -not (Test-Path "$dockerConfig.before-asagiri")) { Copy-Item $dockerConfig "$dockerConfig.before-asagiri" }
$cfg | Add-Member -NotePropertyName cliPluginsExtraDirs -NotePropertyValue $dirs -Force
New-Item -ItemType Directory -Force (Split-Path $dockerConfig) | Out-Null
# UTF-8 without a BOM: Set-Content -Encoding utf8 writes one in Windows
# PowerShell, and docker then rejects the whole file, logins included.
[IO.File]::WriteAllText($dockerConfig, ($cfg | ConvertTo-Json -Depth 32), (New-Object Text.UTF8Encoding $false))
$docker = Join-Path $bin "docker.exe"
$prev = $ErrorActionPreference; $ErrorActionPreference = "Continue"
$out = (& $docker context ls 2>&1 | ForEach-Object { "$_" }) -join "`n"
$ErrorActionPreference = $prev
if ($out -match "Error parsing config") { throw "docker cannot read $dockerConfig after Asagiri changed it (the backup is $dockerConfig.before-asagiri): $out" }
"TASK 1"

if (-not $inPlace) { Copy-Item (Join-Path $Bundle "bin\cli-plugins\docker-buildx.exe") $plugins -Force }
"TASK 2"

Docker context rm -f asagiri | Out-Null
$out = Docker context create asagiri --description "Asagiri (WSL2)" --docker "host=$Pipe"
if ($LASTEXITCODE -ne 0) { throw "could not create the asagiri docker context: $out" }
Docker buildx inspect asagiri-build | Out-Null
if ($LASTEXITCODE -ne 0) {
  $out = Docker buildx create --name asagiri-build --driver docker-container --driver-opt default-load=true asagiri
  if ($LASTEXITCODE -ne 0) { throw "could not create Asagiri's builder: $out" }
}
"TASK 3"

if ($Default) {
  $out = Docker context use asagiri
  if ($LASTEXITCODE -ne 0) { throw "could not make asagiri the default context: $out" }
  # `docker build` then uses the builder named after the context, which
  # asagiri-win makes a copy of Asagiri's (crates/asagiri/src/buildwith.rs): no
  # BUILDX_BUILDER, which would send every build to Asagiri whatever docker uses.
}
"TASK 4"
