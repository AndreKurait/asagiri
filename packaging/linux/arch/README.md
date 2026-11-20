# Arch desktop package

On x86-64 Arch Linux, install `base-devel` and `rustup`. Run from the repository root as an unprivileged build user.

```sh
git archive --format=tar.gz --prefix=asagiri/ \
  -o packaging/linux/arch/asagiri-arch-c9179f64.tar.gz \
  c9179f64d476434e4ba5bcfd0ec5dc1fe9c93818
cd packaging/linux/arch
makepkg --verifysource
rustup toolchain install 1.99.1 --profile minimal
makepkg --syncdeps
```

Configure the local engine separately. The full Asagiri package already owns the desktop paths.
