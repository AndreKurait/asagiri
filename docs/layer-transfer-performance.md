# Layer transfer concurrency

Cold container creation prepares image layers in the guest. Previously, every
create shared one layer lock, and decompression ran on the same connection that
handled container commands. One cold image could therefore delay an unrelated
`exec`, even when the host had spare CPU capacity.

Layer uploads now use separate connections and guest worker processes. The host
allows up to four independent transfers, bounded by its available parallelism.
Requests for the same digest share a lock and recheck whether the layer already
exists. The guest keeps its event loop and container creation path on one thread;
decompression runs outside that loop. Layers within one create are still sent
in sequence.

The implementation preserves these constraints:

- A digest has at most one active transfer from the host.
- Independent transfers cannot exceed the host's capacity limit.
- Failed operations release both the digest lock and the capacity permit.
- A guest file lock protects a digest even across independent upload connections.
- Incomplete transfers and corrupt compressed tails cannot publish a completion
  marker. Files are flushed before a completed layer becomes available.
- A layer worker closes inherited control descriptors and dies if its agent dies.

These constraints have executable concurrency and failure tests. They are not
a formal proof or a claim that every possible schedule has been explored.

## Measurement

The [recorded results](../bench/layer-transfers-2026-11-13.json) compare baseline
`eafd112` with candidate `eae8119`. The workload used three rounds, each creating
four images concurrently, with three compressed layers and 128 files of 8 KiB
per layer. It measured warm creates separately and ran an unrelated `exec`
throughout cold creation.

| Median, milliseconds | Baseline before | Candidate | Baseline after |
| --- | ---: | ---: | ---: |
| Cold batch of four creates | 120.17 | 51.58 | 88.41 |
| Warm create | 0.98 | 1.42 | 1.78 |
| Exec during cold creation | 20.54 | 10.55 | 14.61 |
| Exec while idle | 5.72 | 7.09 | 7.52 |

Cold batches and concurrent exec improved in this workload. Warm results fall
within the observed baseline variation. These are small synthetic measurements
of Linux ARM64 engines in private Docker containers on one host, not application
benchmarks or native macOS VM measurements.

To repeat the workload beside a disposable engine after pulling `alpine:3.21`:

```sh
python3 scripts/bench-layer-concurrency.py --socket /path/to/docker.sock
```

The script removes its containers and image tags. Use separate fresh engines
for baseline and candidate measurements, and avoid compiling or running other
benchmarks at the same time.

`scripts/ci/live-tests.sh` also runs the shared-layer/corrupt-transfer regression,
ordinary container and archive checks, restart recovery, and Buildx.
