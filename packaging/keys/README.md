# RPM repository signing

`asagiri-rpm.asc` is the public key for the RPM repository hosted on GitHub
Pages. Its full fingerprint is in `asagiri-rpm.fingerprint`.

The Website workflow imports the dedicated private key from the
`ASAGIRI_RPM_SIGNING_KEY` Actions secret into a temporary keyring. It verifies
each original release asset's digest and GitHub build provenance, adds an RPM
header signature, verifies that signature, and builds DNF metadata. No private
key or keyring is included in the site artifact.

An RPM from the repository has a different file checksum from the original
release RPM because its signature header was added. Each repository's
`provenance.json` records both checksums and the original source commit.
Package payloads are unchanged. DNF requires a valid package signature.

The installer pins the public key's SHA-256. Key rotation therefore requires
updating both the committed public key and the installer pin before deploying.
