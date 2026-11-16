# Execute only the production prerequisite function, with process creation
# replaced by a local test stub. No feature, MSI, service or registry changes.
$ErrorActionPreference = 'Stop'
$source = Join-Path $PSScriptRoot '..\contrib\windows\asagiri-admin.ps1'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
$functions = @($ast.FindAll({
  param($node)
  $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Install-WslPrerequisites'
}, $true))
if ($functions.Count -ne 1) { throw 'Expected the production WSL prerequisite function' }

function Start-Process {
  param([string]$FilePath, [string[]]$ArgumentList, [switch]$Wait, [switch]$PassThru, [string]$WindowStyle)
  if (-not $Wait -or -not $PassThru -or $WindowStyle -ne 'Hidden') { throw 'Unexpected process options' }
  if (-not $script:ExitCodes.Count) { throw 'Unexpected extra process' }
  $script:Calls.Add([pscustomobject]@{ File = $FilePath; Args = $ArgumentList })
  [pscustomobject]@{ ExitCode = $script:ExitCodes.Dequeue() }
}
Invoke-Expression $functions[0].Extent.Text
$installer = 'C:\fixture path\verified.msi'
$cases = @(
  @{ Name = 'already enabled'; Codes = @(0, 0, 0); Want = 0; Calls = 3 },
  @{ Name = 'platform restart'; Codes = @(3010, 0, 0); Want = 3010; Calls = 3 },
  @{ Name = 'WSL component restart'; Codes = @(0, 3010, 0); Want = 3010; Calls = 3 },
  @{ Name = 'MSI restart'; Codes = @(0, 0, 3010); Want = 3010; Calls = 3 },
  @{ Name = 'all need restart'; Codes = @(3010, 3010, 3010); Want = 3010; Calls = 3 },
  @{ Name = 'platform failed'; Codes = @(5); Want = 20; Calls = 1 },
  @{ Name = 'WSL component failed'; Codes = @(0, 5); Want = 24; Calls = 2 },
  @{ Name = 'MSI failed'; Codes = @(0, 0, 1603); Want = 21; Calls = 3 }
)
foreach ($case in $cases) {
  $script:ExitCodes = [System.Collections.Generic.Queue[int]]::new()
  foreach ($code in $case.Codes) { $script:ExitCodes.Enqueue($code) }
  $script:Calls = [System.Collections.Generic.List[object]]::new()
  $actual = Install-WslPrerequisites -Installer $installer
  if ($actual -ne $case.Want -or $script:Calls.Count -ne $case.Calls -or $script:ExitCodes.Count) {
    throw "Wrong result for $($case.Name): result=$actual calls=$($script:Calls.Count)"
  }
  if ($script:Calls[0].File -notmatch '\\dism.exe$' -or
      ($script:Calls[0].Args -join '|') -ne '/online|/enable-feature|/featurename:VirtualMachinePlatform|/all|/norestart') {
    throw 'Virtual Machine Platform must be enabled first without restarting'
  }
  if ($case.Calls -ge 2 -and
      ($script:Calls[1].File -notmatch '\\dism.exe$' -or
       ($script:Calls[1].Args -join '|') -ne '/online|/enable-feature|/featurename:Microsoft-Windows-Subsystem-Linux|/all|/norestart')) {
    throw 'The Windows Subsystem for Linux component must also be enabled'
  }
  if ($case.Calls -eq 3 -and
      ($script:Calls[2].File -notmatch '\\msiexec.exe$' -or
       ($script:Calls[2].Args -join '|') -ne ('/i|"' + $installer + '"|/qn|/norestart'))) {
    throw 'Install only the verified MSI, preserving its quoted path and restart policy'
  }
  Write-Output "PASS WSL prerequisite commands: $($case.Name)"
}
