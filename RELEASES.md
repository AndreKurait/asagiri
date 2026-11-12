# Asagiri releases

No Asagiri release has been published yet. Build from source using
[README.md](README.md). Platform CI retains unsigned development binaries and
SHA-256 checksums; those artifacts are not complete desktop installers.

The version in Cargo.toml remains a development version until the first
Asagiri release. Historical files in `docs/releases/` describe **Kiri** releases,
not Asagiri releases. They are preserved with attribution.

## Preparing a release

1. Merge reviewed changes with all required checks passing.
2. Update the workspace version and write Asagiri release notes.
3. Build and test packages on Linux, macOS, and Windows; see [packaging](packaging/README.md).
4. Regenerate the brand and demo assets and the third-party notices.
5. Sign platform artifacts where signing is available, publish checksums and
   provenance, and verify installation and upgrades on real target systems.
6. Publish a GitHub release in this repository. Configure update feeds only
   after the published artifacts, identities, and hashes have been verified.

`ASAGIRI_RELEASE_BASE` can select a release artifact location. The package
builder defaults to this repository's versioned GitHub release path.
`ASAGIRI_UPDATE_URL` must be set explicitly to use an update feed. Updater
checks preserve the app/MSI identity and target architecture.
