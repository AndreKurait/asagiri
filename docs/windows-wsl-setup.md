# Windows WSL setup

The elevated WSL setup step enables both Virtual Machine Platform and
`Microsoft-Windows-Subsystem-Linux` before installing the verified WSL MSI.
This addresses [Kiri issue #297](https://github.com/libracoder/kiri/issues/297),
where Windows 10 had the MSI installed but the WSL optional component was
still disabled.

Each step retains its `/norestart` policy. Exit code 3010 from either Windows
feature or the MSI requests the existing restart-and-resume flow. A failed
feature stops installation and reports that failure. The MSI's hash and
Microsoft signature are checked on the protected copy before the helper
enables features or starts the installer.

An existing installation reporting `WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED`
returns to the WSL installation step. A previously approved pending restart
continues to show the restart step. Other WSL status failures do not cause
setup to assume that WSL needs reinstalling.

Setup requests UTF-8 output from its WSL commands. The pinned
[WSL 3.0.3 source](https://github.com/microsoft/WSL/blob/3.0.3/src/windows/common/WslClient.cpp)
implements `WSL_UTF8=1`. The decoder also accepts UTF-16 byte-order marks and
NUL-marked legacy output, including localized messages whose first characters
have nonzero high bytes. The original diagnostic and its error identifier
remain readable in the setup window and log.

Four portable Rust tests execute the output decoder and command environment
handling, including Russian, Chinese, Japanese, Arabic, surrogate pairs and
incomplete output. Windows setup tests check the persisted restart and repair
transitions. `tests/windows-wsl-setup.ps1` executes the production prerequisite
function with process creation replaced by a test stub, checking both feature
commands, their order, all three restart results and early failure handling.
That test is required by the Windows platform job.

The portable tests pass on macOS. The local Windows cross-check was stopped
by the 3 GiB disk guard. Windows job 133110090370 then passed native compilation,
178 Rust tests (two existing tests ignored), all eight prerequisite command
cases and release packaging. The Linux job found a collision between parallel
archive tests' timestamp-based temporary paths; those paths now include an
atomic sequence number. The complete platform run must pass before merge.
Enabling features and completing a real Windows 10 reboot have not been
exercised on this Mac.

## PowerShell shows different images from the dashboard

Compare the engine used by plain `docker` with the Asagiri engine in the same
PowerShell window where the images differ.

```powershell
docker context show
docker version --format '{{.Server.Version}}'
docker --context asagiri version --format '{{.Server.Version}}'
docker --context asagiri image ls
$env:DOCKER_CONTEXT
```

If the explicit Asagiri context shows the expected images, check the value of
`DOCKER_CONTEXT`. A terminal override such as `default` takes precedence over
the context selected in the dashboard. To use the saved context in this shell,
clear the override and check the image list again.

```powershell
Remove-Item Env:DOCKER_CONTEXT -ErrorAction SilentlyContinue
docker context show
docker image ls
```

This clears the variable for the current PowerShell process. If the value
returns in a new terminal, check the environment or profile that sets it.
