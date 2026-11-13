# GLib 0.18 safety backport

This directory contains the unmodified files from the crates.io `glib` 0.18.5
archive, except for the two-line `VariantStrIter::impl_get` fix in
`src/variant_iter.rs`, this provenance file, and the added test dependency
lockfile.

The original archive SHA-256 is
`233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5`.
The upstream license and copyright files are retained.

The fix is from gtk-rs/gtk-rs-core PR 1343, merged as
`05dff0ee696f9bcd8617cd48c4b812d046d440cb`. It passes a mutable output pointer
to `g_variant_get_child` instead of permitting C to mutate through a shared
Rust reference. This addresses RUSTSEC-2024-0429 / GHSA-wrw7-89jp-8q8g.

GTK 3 and WebKitGTK 4.1 require GLib's Rust 0.18 API. Upgrading only GLib to
0.20 would leave their 0.18 dependency in use. The workspace patch ensures
all 0.18 users resolve to this corrected copy.

Run the existing upstream iterator regressions with optimization:

```sh
cargo test --locked --release --manifest-path vendor/glib/Cargo.toml --lib variant_iter::tests
```

The test needs the system GLib development files; it does not open a window.
No RustSec advisory is suppressed for this patch. Remove the backport when
the GTK dependencies can use a fixed upstream GLib release.
