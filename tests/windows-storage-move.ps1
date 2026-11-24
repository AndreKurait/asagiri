$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$root = Join-Path $env:RUNNER_TEMP 'wsl-storage-test'
New-Item -ItemType Directory $root | Out-Null
$asset = 'Asagiri-0.1.14-dev.205-x86_64.msi'
gh release download v0.1.14-dev.205 --repo AndreKurait/asagiri --pattern $asset --dir $root
if ((Get-FileHash "$root\$asset").Hash -ne '1592852ad8ae07a8b903592ec868164697ba49a38629e47de55127da646dc8b2') { throw 'Unexpected base MSI' }
$extract = Start-Process msiexec -ArgumentList "/a `"$root\$asset`" /qn TARGETDIR=`"$root\bundle`"" -Wait -PassThru
if ($extract.ExitCode) { throw "MSI extraction failed with $($extract.ExitCode)" }
$tar = @(Get-ChildItem "$root\bundle" -Filter asagiri-linux.tar.gz -Recurse)
if ($tar.Count -ne 1) { throw 'Expected one distro archive in the MSI' }
$distro = 'AsagiriStorageTest166'
wsl.exe --import $distro "$root\original" $tar[0].FullName --version 2
try {
    wsl.exe -d $distro -u root -- sh -ec 'echo preserved > /root/storage-proof'
    wsl.exe --terminate $distro
    $env:ASAGIRI_WSL_STORAGE_TEST_ROOT = $root
    $output = cargo test --locked -p asagiri-win native_wsl_storage_export_import -- --ignored --nocapture
    $output | Write-Host
    if (($output -join "`n") -notmatch '1 passed; 0 failed') { throw 'Migration test did not execute' }
    $value = wsl.exe -d $distro -u root -- cat /root/storage-proof
    if ($value.Trim() -ne 'preserved') { throw 'Migrated distro lost its marker' }
} finally {
    wsl.exe --unregister $distro
}
