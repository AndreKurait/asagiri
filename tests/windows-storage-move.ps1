$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$root = Join-Path $env:RUNNER_TEMP 'wsl-storage-test'
New-Item -ItemType Directory $root | Out-Null
$asset = 'Asagiri-0.1.14-dev.205-x86_64.msi'
Write-Host "$(Get-Date -Format o) Downloading the pinned MSI"
gh release download v0.1.14-dev.205 --repo AndreKurait/asagiri --pattern $asset --dir $root
if ((Get-FileHash "$root\$asset").Hash -ne '1592852ad8ae07a8b903592ec868164697ba49a38629e47de55127da646dc8b2') { throw 'Unexpected base MSI' }
Write-Host "$(Get-Date -Format o) Extracting the distro archive"
$extract = Start-Process msiexec -ArgumentList "/a `"$root\$asset`" /qn TARGETDIR=`"$root\bundle`"" -Wait -PassThru
if ($extract.ExitCode) { throw "MSI extraction failed with $($extract.ExitCode)" }
$tar = @(Get-ChildItem "$root\bundle" -Filter asagiri-linux.tar.gz -Recurse)
if ($tar.Count -ne 1) { throw 'Expected one distro archive in the MSI' }
$distro = 'AsagiriStorageTest166'
Write-Host "$(Get-Date -Format o) Importing the source distro"
wsl.exe --import $distro "$root\original" $tar[0].FullName --version 2
try {
    Write-Host "$(Get-Date -Format o) Booting the source distro"
    wsl.exe -d $distro -u root -- sh -ec 'echo preserved > /root/storage-proof'
    Write-Host "$(Get-Date -Format o) Terminating the source distro"
    wsl.exe --terminate $distro
    $env:ASAGIRI_WSL_STORAGE_TEST_ROOT = $root
    $env:ASAGIRI_WSL_STORAGE_TEST_TAR = $tar[0].FullName
    Write-Host "$(Get-Date -Format o) Running native WSL migration tests"
    cargo test --locked -p asagiri-win native_wsl_storage_export_import -- --ignored --nocapture | Tee-Object -Variable output | Out-Host
    if ($LASTEXITCODE) { throw "Migration test failed with $LASTEXITCODE" }
    if (($output -join "`n") -notmatch '1 passed; 0 failed') { throw 'Migration test did not execute' }
    $backupDistro = 'AsagiriStorageBackup166'
    Write-Host "$(Get-Date -Format o) Importing the independent backup"
    wsl.exe --import $backupDistro "$root\restored-backup" "$root\settings-move.asagiri-backup\distro.tar" --version 2
    try {
        Write-Host "$(Get-Date -Format o) Checking the restored backup marker"
        $value = wsl.exe -d $backupDistro -u root -- cat /root/storage-proof
        if ($value.Trim() -ne 'preserved') { throw 'Fresh import backup lost its marker' }
    } finally {
        wsl.exe --unregister $backupDistro
    }
} finally {
    wsl.exe --unregister $distro
}
