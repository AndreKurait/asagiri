# Signing and notarizing macOS releases

The release workflow supports Developer ID signing and Apple notarization for
both Apple Silicon and Intel. It uses a temporary runner keychain, submits to
Apple with `notarytool`, and validates the final app and disk image without
launching the app or its VM.

Without Apple credentials, packages are explicitly ad hoc signed and not
notarized. A valid checksum or ad hoc signature does not mean Apple has
approved a download. ZIP and tar compression do not replace notarization or
reliably avoid quarantine. Keep the DMG as the graphical installer and the
tar archive for the verified command-line installer.

## One-time account setup

This requires an Apple Developer Program account and its own **Developer ID
Application** certificate, including the private key. Export that identity
from Keychain Access as a password-protected `.p12` file. An Apple Development
certificate or a Developer ID Installer certificate is not interchangeable
with a Developer ID Application certificate.

Create an App Store Connect team API key authorized for notarization, and
save its `.p8` private key, key ID, and issuer ID. Follow Apple's current
[Developer ID](https://developer.apple.com/developer-id/) and
[notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
instructions for account permissions and credential creation.

Store credentials directly in GitHub Actions secrets; do not put them in an
issue, a chat, a commit, or a workflow file. From a trusted terminal:

```sh
base64 -i /secure/path/DeveloperID.p12 |
  gh secret set ASAGIRI_APPLE_CERTIFICATE_BASE64 --repo AndreKurait/asagiri
gh secret set ASAGIRI_APPLE_CERTIFICATE_PASSWORD --repo AndreKurait/asagiri

base64 -i /secure/path/AuthKey.p8 |
  gh secret set ASAGIRI_APPLE_API_KEY_BASE64 --repo AndreKurait/asagiri
gh secret set ASAGIRI_APPLE_API_KEY_ID --repo AndreKurait/asagiri
gh secret set ASAGIRI_APPLE_API_ISSUER_ID --repo AndreKurait/asagiri
```

The commands without piped input prompt for their secret values. After all
five secrets are present, prevent a future missing-secret configuration from
silently falling back to ad hoc signing:

```sh
gh variable set ASAGIRI_REQUIRE_APPLE_SIGNING --body true --repo AndreKurait/asagiri
```

Never commit the `.p12` or `.p8` files. Retain them in your protected credential
storage for rotation or recovery.

## Automatic release behavior

On a release using this workflow revision, each Mac runner:

1. Imports the certificate into a temporary keychain and checks for exactly
   one valid Developer ID Application identity.
2. Stores notarization credentials in that keychain and deletes the imported
   private-key files.
3. Signs the bundled tools, engine, and app with the hardened runtime and
   secure timestamps.
4. Submits the app to Apple and staples the accepted ticket to the app.
5. Builds the DMG from that stapled app, submits the final DMG to Apple, and
   staples and validates the DMG's own ticket.
6. Assesses both artifacts with Gatekeeper, then repacks the stapled app and
   regenerates the tar archive's checksum and update feed.
7. Deletes the temporary keychain and restores the runner's original keychain
   search list, including after a failed build.

An incomplete credential set, rejected certificate, rejected notarization,
or failed Gatekeeper assessment fails packaging. Rejection diagnostics are
retained as a workflow artifact; credentials are not.

Previously created version tags keep their original workflow and source.
Publish a new version tag after the signing workflow is on `main` and the
credentials are configured. Do not move an existing release tag.

GitHub provenance and SHA-256 checks remain separate from Apple signing:
they identify the build and download integrity, while Developer ID and
notarization establish macOS distribution trust.
