# Windows build distribution

Local `cargo xtask windows`, `windows-dev`, and `msi` builds select one user WSL
distribution for path conversion and both Linux build scripts. Discovery skips
Docker Desktop's internal distributions and the Asagiri/Kiri runtime
distributions, including their development flavors. Those runtime images are
not general development environments.

Set `ASAGIRI_WSL_DISTRO` to choose a specific distribution with the build
dependencies installed. The override takes precedence over discovery. It does
not change WSL's default distribution. Release packaging that supplies
`ASAGIRI_WINDOWS_LINUX_DIR` continues to use its prebuilt Linux inputs.

The commands request UTF-8 output and also accept legacy UTF-16 distribution
lists. Unicode distribution names survive decoding. A failed path conversion
stops the build with the WSL error instead of passing an empty path to a script.
Each program executes directly in the chosen distribution.

This adapts AD PAO's public
[Kiri PR #314](https://github.com/libracoder/kiri/pull/314),
commit `d0d373ec1cb7cada5bcccee51b6ef19c8d2ddb12`, without its unrelated formatting
changes. Six portable Rust tests cover selection, overrides, encodings,
malformed output, and consistent command scoping. Windows CI runs these with
the application tests. A full local Windows/WSL development build has not been
run as part of this adaptation.
