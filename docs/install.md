# Install Asagiri

The [installation page](https://andrekurait.github.io/asagiri/install/) has
direct download buttons and copy buttons for these commands. Recommended
installs use the latest stable release when available, otherwise the latest
tested prerelease. Downloads are available after the first release publishes.

## macOS

Requires macOS 14 or newer, on Apple Silicon or Intel.

Open the installation page, choose your Mac, open the DMG, and drag Asagiri
to Applications. Open Asagiri to complete its setup.

For Homebrew:

```sh
brew install --cask andrekurait/asagiri/asagiri
open -a Asagiri
```

Without Homebrew, this command detects your architecture, verifies the archive
and app signature, and installs into Applications:

```sh
curl -fsSL https://andrekurait.github.io/asagiri/install.sh | sh
```

The script uses your personal Applications directory when the system directory
is not writable. It prints the exact command to open the installed app.
It also detects Apple Silicon when the terminal runs under Rosetta.

Current apps are ad hoc signed and are not notarized by Apple. If macOS asks
for approval, use System Settings → Privacy & Security → Open Anyway. The
installer does not disable Gatekeeper or remove quarantine flags.
ZIP or tar compression does not replace Apple signing and notarization.
The automated signing setup is documented in [macOS release signing](macos-signing.md).

Update a Homebrew install with:

```sh
brew upgrade --cask andrekurait/asagiri/asagiri
```

For a direct install, quit Asagiri and rerun the install command. Container
data is retained.

## Amazon Linux 2023

Supports x86_64 and aarch64, including Graviton. Run this as the account that
will use containers:

```sh
curl -fsSL https://andrekurait.github.io/asagiri/install.sh | sh
```

Setup asks for sudo, imports the pinned RPM signing key, enables the Asagiri
DNF repository, installs the engine and Docker CLI/Compose/Buildx, starts the
service, and gives your account access. Container access is equivalent to
root access on the host. Existing Docker packages and your selected context
retain their settings.

Try it:

```sh
docker --context asagiri run --rm alpine echo hello
docker --context asagiri compose version
docker --context asagiri buildx version
```

Updates use DNF:

```sh
sudo dnf --refresh upgrade asagiri
```

Package upgrades keep the existing engine and containers running. To activate
the new binary when convenient:

```sh
sudo systemctl restart asagiri
```

Restarting stops its containers; those with a restart policy start again.
Inspect service problems with `journalctl -u asagiri`.

For an AMI or fleet image, install without starting the engine:

```sh
curl -fsSL https://andrekurait.github.io/asagiri/install.sh | sh -s -- --no-start
```

After boot, enable access for the intended account:

```sh
sudo asagiri host-access ec2-user
```

The package's own Docker wrapper already selects Asagiri when no Docker
context has been chosen. For an explicit context in that account's shell:

```sh
docker context create asagiri --docker host=unix:///run/asagiri/docker.sock
```

## Choose a release channel

To follow tested main builds even after stable releases exist:

```sh
curl -fsSL https://andrekurait.github.io/asagiri/install.sh | sh -s -- --channel preview
```

Use `--channel stable` to require a stable release. It reports that no package
is available until the first stable version publishes. `--dry-run` prints the
selected installation path without installing or starting anything.
Homebrew follows the recommended channel.

## Verify or remove

GitHub releases include `SHA256SUMS`, `release-manifest.json`, and GitHub build
provenance. The RPM repository additionally requires a valid package signature;
the setup script pins its public key. See [RPM signing](../packaging/keys/README.md)
for the fingerprint and the relationship between release and signed-repository
checksums.

Uninstall with `brew uninstall --cask andrekurait/asagiri/asagiri` or
`sudo dnf remove asagiri`. For a direct Mac install, quit the app and move it to
Trash. These operations preserve container data.

Windows installers and other Linux packages are on the
[release page](https://github.com/AndreKurait/asagiri/releases).
