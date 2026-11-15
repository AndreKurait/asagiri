# Registry sign-in on Linux

The window can save a registry sign-in even when no credential helper is
configured. It uses the Docker configuration directory from `DOCKER_CONFIG`,
or `~/.docker` when that variable is unset or empty.

Asagiri checks a registry's `credHelpers` entry before the global `credsStore`.
A configured helper must be available and accept the sign-in. Its failure is
reported; it does not cause the password to be saved in a file instead.

For a fresh configuration without authentication settings, Asagiri follows
Docker's Linux default selection: an executable `pass` program selects
`docker-credential-pass`; otherwise it selects
`docker-credential-secretservice`. If that selected helper is unavailable,
Asagiri saves the sign-in in `config.json`. A detected helper is recorded in
`credsStore` so the Docker CLI can find the saved credentials.

The file fallback uses mode `0600`. Password credentials are base64 encoded,
and identity tokens are stored directly; neither is encrypted. Configure a
credential helper to use its protected store.

Saving replaces the selected registry's old aliases while preserving other
registries, contexts, and settings. Invalid configurations are left intact.
Temporary files are private before credentials are written, and the completed
file replaces the old file atomically. Existing configuration symlinks are
preserved. Helper output that might contain a password is excluded from
sign-in errors.

macOS continues to use Keychain, and Windows continues to use Credential
Manager when neither has a configured helper.

This fixes the Linux window failure reported in
[Kiri #292](https://github.com/libracoder/kiri/pull/292), with additional
configuration and helper regressions.
