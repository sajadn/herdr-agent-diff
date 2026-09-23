# Release profile check

Measured on the target arm64 Mac on 2026-07-30 with Rust 1.97.1, fat LTO, one
codegen unit, stripped symbols, and abort-on-panic:

| Profile | Binary | 50 cold `status` launches |
| --- | ---: | ---: |
| `opt-level = "s"` | 2.3 MiB | 0.49 s |
| `opt-level = "3"` | 2.7 MiB | 0.42 s |

The approximately 1.4 ms difference per cold process launch is immaterial to
viewer interaction, while `"s"` reduces the binary by about 15%. The release
profile therefore keeps `"s"`.

Interactive work is kept off the input/render loop by the bounded worker in
`src/app.rs`. Snapshot generations discard obsolete scan results, filesystem
bursts are debounced for 150 ms, and rendered content is retained in a
32 MiB byte-bounded cache.

## Remote Linux interaction check

On 2026-09-23, a 240-column by 60-row PTY running a warmed historical Files
view on a Linux login node gave these before/after measurements:

| Measurement | Before | After |
| --- | ---: | ---: |
| Viewer CPU over a 3-second idle interval (one core = 100%) | 5.67% | 0.33% |
| Process 100 queued Down keys and exit through the commit picker | 0.200 s | 0.089 s |

These are single-run viewer-process measurements, not end-to-end SSH latency.
The event loop now redraws after state changes instead of every 50 ms, batches
scroll events (at most 32 events or 8 ms), and checks source-pane liveness on the
worker thread. Stored scroll offsets are clamped after each event so batching
does not reintroduce overscroll at file boundaries. Working-tree notifications
also no longer refresh immutable commit reviews.
