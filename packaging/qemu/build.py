#!/usr/bin/env python3
"""Build a static Intel Linux emulator and retain all corresponding sources."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import tarfile
import time
import urllib.request

RECIPE = Path(__file__).resolve().parent
PINS = json.loads((RECIPE / "sources.json").read_text())
RECIPE_FILES = ("build.py", "sources.json", "asagiri_x86_64_defconfig", "probe.c", "README.md", "SOURCE.md")
MIN_FREE = 3 * 1024**3


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def run(*args, **kwargs):
    print("+", " ".join(str(a) for a in args), flush=True)
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def guard(path):
    require(shutil.disk_usage(path).free >= MIN_FREE, "Build guard: fewer than 3 GiB free")


def download(pin, path):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_file() and sha(path) == pin["sha256"]:
        return
    partial = path.with_name(path.name + ".partial")
    for attempt in range(3):
        try:
            with urllib.request.urlopen(pin["url"], timeout=120) as response, partial.open("wb") as out:
                shutil.copyfileobj(response, out)
            require(sha(partial) == pin["sha256"], "Source checksum mismatch: " + path.name)
            partial.replace(path)
            return
        except Exception:
            partial.unlink(missing_ok=True)
            if attempt == 2:
                raise
            time.sleep(attempt + 1)


def buildroot_member(member, destination):
    # This unused OpenRC skeleton link points outside the extraction tree.
    # Keep the original in the source archive, but do not create it on the
    # builder. Our configuration selects no init system or root filesystem.
    unused = ("buildroot-" + PINS["buildroot"]["version"]
              + "/package/skeleton-init-openrc/skeleton/etc/runlevels/default/sysv-rcs")
    if member.name == unused and member.issym() and member.linkname == "/etc/init.d/sysv-rcs":
        return None
    return tarfile.data_filter(member, destination)


def static_x86_64(path):
    with path.open("rb") as source:
        header = source.read(64)
        require(len(header) == 64 and header[:6] == b"\x7fELF\x02\x01", "Expected a little-endian ELF64 emulator")
        require(struct.unpack_from("<H", header, 18)[0] == 62, "Emulator must run on x86_64 Linux")
        offset = struct.unpack_from("<Q", header, 32)[0]
        size, count = struct.unpack_from("<HH", header, 54)
        require(size == 56 and 0 < count < 128, "Invalid ELF program headers")
        length = os.fstat(source.fileno()).st_size
        require(offset + size * count <= length, "Truncated ELF program headers")
        source.seek(offset)
        dynamic = []
        for _ in range(count):
            program = source.read(size)
            require(len(program) == size, "Truncated ELF program header")
            kind, _, start, _, _, length_on_disk, _, _ = struct.unpack("<IIQQQQQQ", program)
            require(kind != 3, "Emulator requires a dynamic interpreter")
            if kind == 2:
                require(length_on_disk <= 1 << 20 and length_on_disk % 16 == 0
                        and start + length_on_disk <= length, "Invalid ELF dynamic section")
                dynamic.append((start, length_on_disk))
        for start, length_on_disk in dynamic:
            source.seek(start)
            for _ in range(length_on_disk // 16):
                tag, _ = struct.unpack("<QQ", source.read(16))
                require(tag != 1, "Emulator requires shared libraries")
                if tag == 0:
                    break


def legal_sources(directory, buildroot_archive):
    """Reject an incomplete legal-info result before distributing the binary."""
    require(sha(buildroot_archive) == PINS["buildroot"]["sha256"], "Missing verified Buildroot source")
    run("sha256sum", "--check", "legal-info.sha256", cwd=directory, stdout=subprocess.DEVNULL)
    warnings = [line.strip() for line in (directory / "README").read_text().splitlines()
                if line.strip().startswith("WARNING:")]
    require(warnings == ["WARNING: the Buildroot source code has not been saved"],
            "Unresolved source collection warnings: " + repr(warnings))
    packages = []
    for manifest, sources, licenses in (
        ("manifest.csv", "sources", "licenses"),
        ("host-manifest.csv", "host-sources", "host-licenses"),
    ):
        with (directory / manifest).open(newline="") as stream:
            rows = list(csv.DictReader(stream))
        require(rows, "Empty source manifest: " + manifest)
        for row in rows:
            name = row["PACKAGE"]
            require(row["LICENSE"] and "unknown" not in row["LICENSE"].lower(),
                    "Missing license: " + name)
            require(row["LICENSE FILES"] and row["LICENSE FILES"] != "not saved",
                    "Missing license files: " + name)
            if name == "buildroot":
                require(manifest == "host-manifest.csv" and row["VERSION"] == PINS["buildroot"]["version"],
                        "Unexpected Buildroot source version")
                continue  # The verified archive is supplied separately.
            archive = row["SOURCE ARCHIVE"]
            require(archive and Path(archive).name == archive and archive != "not saved",
                    "Missing source archive: " + name)
            folder = directory / sources / (name + "-" + row["VERSION"])
            require((folder / archive).is_file(), "Source archive was not collected: " + name)
            for filename in row["LICENSE FILES"].split():
                require(not Path(filename).is_absolute() and ".." not in Path(filename).parts,
                        "Unsafe license path")
                require((directory / licenses / (name + "-" + row["VERSION"]) / filename).is_file(),
                        "License was not collected: " + name + "/" + filename)
            packages.append({"name": name, "version": row["VERSION"], "source": archive})
    require(any(p["name"] == "qemu" and p["version"] == PINS["qemu"]["version"] for p in packages),
            "QEMU source is missing")
    require(any(p["name"] == "musl" for p in packages)
            and any(p["name"] in ("gcc", "gcc-final") for p in packages), "Internal toolchain source is missing")
    return packages


def copy_link(source, destination):
    try:
        os.link(source, destination)
    except OSError:
        shutil.copy2(source, destination)
    return destination


def checksums(directory):
    lines = []
    for file in sorted(directory.rglob("*")):
        require(not file.is_symlink(), "Source bundle contains a symlink: " + str(file))
        if file.is_file() and file.name != "SHA256SUMS":
            lines.append(sha(file) + "  " + str(file.relative_to(directory)))
    (directory / "SHA256SUMS").write_text("\n".join(lines) + "\n")


def pack(directory, output, compression):
    run("tar", "--sort=name", "--mtime=@" + str(PINS["source_date_epoch"]),
        "--owner=0", "--group=0", "--numeric-owner", "-c" + compression + "f", output,
        "-C", directory, ".", env={**os.environ, "XZ_OPT": "-T2 -3"})


def verify(output):
    record = json.loads((output / "build.json").read_text())
    require(record["recipe"] == {name: sha(RECIPE / name) for name in RECIPE_FILES},
            "Build products use a different recipe")
    require(record["sources"] == PINS, "Build products use different source pins")
    for filename, digest in record["artifacts"].items():
        require(Path(filename).name == filename and sha(output / filename) == digest,
                "Build product checksum mismatch")
    require(set(record["artifacts"]) == {"asagiri-qemu-guest.tar.gz", "asagiri-qemu-source.tar.xz"},
            "Incomplete QEMU build products")
    print("Verified pinned QEMU build products and complete-source artifact", flush=True)


def build(work, output, source_bundle=None):
    require(platform.system() == "Linux" and platform.machine() == "x86_64",
            "Build QEMU on x86_64 Linux to produce the Intel macOS guest bundle")
    work.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    guard(work)
    download_dir = work / "downloads"
    if source_bundle:
        run("sha256sum", "--check", "SHA256SUMS", cwd=source_bundle, stdout=subprocess.DEVNULL)
        shutil.copytree(source_bundle / "downloads", download_dir, copy_function=copy_link)
    br = PINS["buildroot"]
    archive = work / ("buildroot-" + br["version"] + ".tar.xz")
    if source_bundle:
        shutil.copy2(source_bundle / archive.name, archive)
    download(br, archive)
    with tarfile.open(archive) as source:
        source.extractall(work, filter=buildroot_member)
    tree = work / ("buildroot-" + br["version"])
    recipe = tree / "package/qemu/qemu.mk"
    text = recipe.read_text()
    require(text.count("QEMU_VERSION = 11.0.0\n") == 1, "Review the changed Buildroot QEMU recipe")
    recipe.write_text(text.replace("QEMU_VERSION = 11.0.0\n", "QEMU_VERSION = " + PINS["qemu"]["version"] + "\n"))
    qemu_name = "qemu-" + PINS["qemu"]["version"] + ".tar.xz"
    with (tree / "package/qemu/qemu.hash").open("a") as hashes:
        hashes.write("\n# Asagiri: verified official stable release signature.\nsha256  "
                     + PINS["qemu"]["sha256"] + "  " + qemu_name + "\n")
    download(PINS["qemu"], download_dir / "qemu" / qemu_name)
    build_dir = work / "output"
    require(not build_dir.exists(), "Use a fresh work directory for each build")
    make = ("make", "-C", tree, "O=" + str(build_dir), "BR2_DL_DIR=" + str(download_dir),
            "BR2_JLEVEL=" + str(min(os.cpu_count() or 2, 4)))
    run(*make, "BR2_DEFCONFIG=" + str(RECIPE / "asagiri_x86_64_defconfig"), "defconfig")
    config = (build_dir / ".config").read_text()
    for key in ("BR2_x86_64", "BR2_TOOLCHAIN_BUILDROOT_MUSL", "BR2_STATIC_LIBS",
                "BR2_DOWNLOAD_FORCE_CHECK_HASHES", "BR2_PACKAGE_QEMU_LINUX_USER",
                "BR2_PACKAGE_QEMU_TARGET_AARCH64"):
        require(key + "=y\n" in config, "Required configuration was dropped: " + key)
    run(*make, "legal-info")
    packages = legal_sources(build_dir / "legal-info", archive)
    guard(work)
    run(*make, "qemu")
    guard(work)
    binary_dir = work / "guest/qemu"
    binary_dir.mkdir(parents=True)
    emulator = binary_dir / "qemu-aarch64"
    shutil.copy2(build_dir / "target/usr/bin/qemu-aarch64", emulator)
    run(build_dir / "host/bin/x86_64-buildroot-linux-musl-strip", "--strip-unneeded", emulator)
    static_x86_64(emulator)
    version = run(emulator, "--version", text=True, capture_output=True).stdout
    require("version " + PINS["qemu"]["version"] in version, "Wrong built emulator version")
    probe = work / "probe-aarch64"
    run("aarch64-linux-gnu-gcc", "-static", "-O2", "-pthread", "-Wall", "-Wextra", "-Werror",
        RECIPE / "probe.c", "-o", probe)
    run(emulator, probe, "asagiri-arm64-probe", cwd=work, timeout=120)
    record = {"schema": 1, "sources": PINS, "recipe": {name: sha(RECIPE / name) for name in RECIPE_FILES},
              "configuration_sha256": sha(build_dir / ".config"), "emulator_sha256": sha(emulator),
              "packages": packages, "version": version.strip(),
              "compiler": run(build_dir / "host/bin/x86_64-buildroot-linux-musl-gcc", "--version",
                              text=True, capture_output=True).stdout.splitlines()[0]}
    source_dir = work / "complete-source"
    source_dir.mkdir()
    shutil.copy2(archive, source_dir / archive.name)
    for folder in ("downloads", "output/legal-info"):
        shutil.copytree(work / folder, source_dir / Path(folder).name, copy_function=copy_link,
                        ignore=shutil.ignore_patterns(".lock", "*.lock", "*.partial"))
    (source_dir / "recipe").mkdir()
    for name in RECIPE_FILES:
        shutil.copy2(RECIPE / name, source_dir / "recipe" / name)
    shutil.copy2(build_dir / ".config", source_dir / "buildroot.config")
    (source_dir / "build.json").write_text(json.dumps(record, indent=2) + "\n")
    shutil.copy2(build_dir / ("build/qemu-" + PINS["qemu"]["version"] + "/COPYING"), binary_dir / "COPYING")
    shutil.copy2(RECIPE / "README.md", binary_dir / "README.md")
    shutil.copy2(RECIPE / "SOURCE.md", binary_dir / "SOURCE.md")
    shutil.copy2(build_dir / "legal-info/manifest.csv", binary_dir / "manifest.csv")
    shutil.copytree(build_dir / "legal-info/licenses", binary_dir / "licenses")
    (binary_dir / "build.json").write_text(json.dumps(record, indent=2) + "\n")
    checksums(binary_dir)
    checksums(source_dir)
    guard(work)
    pack(work / "guest", output / "asagiri-qemu-guest.tar.gz", "z")
    pack(source_dir, output / "asagiri-qemu-source.tar.xz", "J")
    record["artifacts"] = {name: sha(output / name) for name in (
        "asagiri-qemu-guest.tar.gz", "asagiri-qemu-source.tar.xz")}
    (output / "build.json").write_text(json.dumps(record, indent=2) + "\n")
    verify(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("build", "verify"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--work", type=Path)
    parser.add_argument("--source-bundle", type=Path)
    args = parser.parse_args()
    if args.command == "verify":
        verify(args.output.resolve())
    else:
        require(args.work is not None, "--work is required")
        build(args.work.resolve(), args.output.resolve(),
              args.source_bundle.resolve() if args.source_bundle else None)


if __name__ == "__main__":
    main()
