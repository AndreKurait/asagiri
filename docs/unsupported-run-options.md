# Unsupported container options

This adapts [public Kiri PR #265](https://github.com/libracoder/kiri/pull/265), commit [`40a2b3c1ef8cfd354e87110928ea54379937eaf5`](https://github.com/libracoder/kiri/commit/40a2b3c1ef8cfd354e87110928ea54379937eaf5), by Arekatla Nishanth Chowdary.

Container creation rejects requested namespace, runtime, isolation, inherited-volume, and storage modes that Asagiri cannot provide. Empty client defaults remain accepted, as do the existing private namespaces, shareable IPC, default isolation, and `asagiri`/`runc` runtime names. These runtime names select Asagiri's existing behavior; they do not launch an external runtime. Validation runs before implicit volume creation.

Unsupported resource controls return warnings on both create and update: memory swappiness, OOM score adjustment, realtime CPU settings, and block-device weights or rate limits. Existing memory and swap limits, memory reservations, CPU sets, and the supported Buildx cgroup parent remain implemented and do not produce these warnings. The existing `OomKillDisable` warning is retained. A remote log driver returns a warning that its logs are retained locally and are not sent to that driver.

The regression was reproduced against the installed `v0.1.6-dev.99`: a request for `PidMode=host` returned HTTP 201 and created its associated named volume. The labelled test container and volume were removed and independently returned HTTP 404 afterward.

With Rust 1.99.1 on macOS, the combined candidate passed all 721 workspace tests, with four existing ignored tests. These include validation of unsupported modes, client defaults, malformed values, resource warnings on create and update, and remote log-driver warnings. Shell syntax and ShellCheck error checks passed.

The required Linux live suite now includes `tests/run-options.py`. It checks HTTP rejection before volume side effects, preserved supported limits, each warning on create and update, unchanged configuration after invalid updates, actual local log retrieval, and fixture removal. Its positive result must be verified on the published candidate; the macOS unit tests and the dev.99 negative reproduction do not establish that result.
