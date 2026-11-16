# Image labels in disk usage

`GET /system/df` includes each image's labels, using the same label lookup as
image listing. This lets Docker API clients retain image metadata while
displaying disk usage. Unlabelled images return `Labels: null`.

This is a partial port of
[Kiri commit 711428d](https://github.com/libracoder/kiri/commit/711428d70e6e04319cfad558de583c1d61279cdb)
from [PR #241](https://github.com/libracoder/kiri/pull/241), addressing
[issue #206](https://github.com/libracoder/kiri/issues/206). Asagiri retains its
existing label cache. Older image records without cached labels read their
immutable config blob without rewriting the index. A missing legacy config
does not prevent the disk-usage response.

`tests/image-label-inheritance.py` loads uniquely named, zero-layer labelled
and unlabelled images. It compares their labels across inspect, listing and
disk usage, then checks container inheritance and filters. The fixture never
starts its containers and removes only its own resources. The restart mode
checks the same image and container metadata after restarting the private CI
engine. `scripts/ci/live-tests.sh` runs the fixture against the runner's Docker
daemon before running it against Asagiri.

Local image-store tests cover cached labels, legacy config lookup without
index mutation, and cached labels remaining readable when the blob is absent.
An installed Asagiri 0.1.5 negative control returned `Labels: null` for the
labelled image in disk usage while inspect and listing returned its labels;
the fixture removed both test images. Hosted Docker comparison and the
updated engine's live result remain pending until the branch's platform
workflow completes.
