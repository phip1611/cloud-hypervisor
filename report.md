# Cloud Hypervisor: Performance and Lifecycle Assessment

Assessment of typical cloud VM scenarios (boot, reboot, shutdown, pause,
hotplug, virtio-blk with raw images, virtio-net with tap) on Cloud Hypervisor
HEAD `acc8801a1` (`v53.0-591`), 2026-09-26/27.

## TL;DR

- **virtio-blk has a real bug**: after draining a queue, guest notifications
  are not re-armed (EVENT_IDX). Low queue depth I/O on one queue gets
  serialized. A ~10 line fix doubles IOPS at QD2 on an SSD (49% -> 85% of
  host-native). This is the most valuable fix found.
- **virtio-net small packets towards the guest are lost**: 77% loss for 64 B
  UDP host->guest (0.31 Mpps), because neither `MRG_RXBUF` nor `INDIRECT_DESC`
  is offered. The ceiling with small RX buffers is 4x higher. Offering
  `INDIRECT_DESC` alone gives +35% (trivial).
- **The virtio-net rate limiter applies per queue pair**: a configured
  1 Gbit/s becomes 4 Gbit/s with 4 queue pairs. Undocumented.
- **Queue defaults are fine, docs are missing**: more queues help only with
  parallel I/O in the guest (up to 4.3x for virtio-blk on a fast backend,
  +37% for virtio-net with multiple flows) and cost host threads and CPU.
  A minimal tuning document is proposed below.
- **Smaller issues**: one leaked fd per VM creation/reboot, a default
  `--net` that fails on multi-queue taps, 1 ms polling in pause/resume, an
  HTTP 500 on quick device re-add, a few unverified correctness issues.
- Everything else looked healthy (see [Healthy results](#healthy-results)).

The full list of action items with estimated benefit and ease is in
[Action items](#action-items).

## Contents

1. [Methodology](#methodology)
2. [virtio-blk (raw images)](#virtio-blk-raw-images)
3. [virtio-net (tap)](#virtio-net-tap)
4. [General / VM lifecycle](#general--vm-lifecycle)
5. [Plan: performance tuning document](#plan-performance-tuning-document)
6. [Action items](#action-items)

Legend used in this report:

- **Benefit** (1-10): how much users would notice a fix. 10 = large and
  broad effect.
- **Ease**: trivial (< ~10 LOC or docs), small (< ~50 LOC), medium, large.
- **Evidence**: *measured* (macro benchmark), *prototyped* (fix implemented
  and A/B-tested against HEAD), *code* (found by code reading only, not
  measured).

## Methodology

### Setup

| Item | Value |
|---|---|
| Host | Intel i5-10600K (6C/12T), 31 GiB, Linux 7.2.8, `mitigations=off`, THP `madvise`, 4 GiB of 2 MiB hugepages |
| Storage | SATA SSD (Samsung 850 PRO, ext4, ~100k IOPS / ~550 MB/s), tmpfs (`/dev/shm`) |
| Network | host-local: tap devices, `iperf3` server on the host |
| CHV | HEAD `acc8801a1`, release build, default features (`kvm`, `io_uring`) |
| Guest kernel | `nixos-configs` bootitems, "stable" 6.18.53, minimal config |
| Guest userspace | bootitems "minimal" initrd plus `fio` 3.41, `iperf3` 3.20, `socat` |
| Boot | direct kernel boot (bzImage), `console=ttyS0`, `prefault=on` |

### Harness

- A small Python runner starts CHV, timestamps guest serial output on
  arrival, and drives the guest through a bash control channel over
  virtio-vsock (so benchmark traffic is not perturbed). The guest init prints
  a `BENCH_READY` marker once userspace is up.
- Timing: process spawn, serial markers, process exit, CHV log timestamps
  (`-v`) and `--event-monitor` events.
- Per-thread host CPU usage of the VMM (queue threads, io_uring workers,
  vCPUs) is sampled from `/proc/<pid>/task/*/stat` during each run.
- virtio-blk: `fio` in the guest (`libaio`, `direct=1`, 4 KiB random / 1 MiB
  sequential, 5-8 s runs after ramp-up) against `/dev/vda`. The raw image lives
  on tmpfs (VMM overhead visible) or on the SATA SSD (realistic latency). The
  host-native ceiling is `fio` with the same job on the same file.
- virtio-net: `iperf3` (TCP `-t 8 -O 2`, UDP `-u -l 64 -b 0`), guest as
  client, `-R` for host->guest. Runs inside an unprivileged network namespace
  (`unshare -rn`) with CHV-created taps, so the default `num_queues=2` can be
  tested (see [N3](#n3-default---net-fails-on-a-pre-created-multi-queue-tap)).
  Host loopback `iperf3` serves as rough reference.
- Fixes were prototyped in a separate worktree and compared A/B against HEAD,
  alternating the binaries between repetitions. Data integrity of the
  virtio-blk prototype was checked with `fio --verify=crc32c`.
- Medians of 2-5 repetitions; code-reading findings were cross-checked in
  the source and with `git log`/`git blame`.

### Limitations

- One desktop host. Absolute numbers are host-specific; relative results and
  mechanisms are what matters.
- Network tests are host-local: `iperf3` on the host competes with the VM for
  the same 12 host threads, which caps multi-queue scaling.
- No NVMe device; tmpfs stands in for "fast backend".
- `perf`/`strace` on the VMM were not available (`perf_event_paranoid=2`),
  so syscall-level claims come from code reading.
- Noise is about +-10% for single network runs.

## virtio-blk (raw images)

**TL;DR**: One real bug (missing notification re-arm) costs up to 2x at low
queue depth on real devices; the fix is small and prototyped. io_uring is the
default backend and the right one; sync would be 10x slower on an SSD.
Multiple queues pay off only for parallel guest I/O on fast backends, so this
is a documentation topic rather than a bad default.

### B1: Missing EVENT_IDX re-arm serializes low queue depth I/O

*Benefit 8, ease small (~10 LOC), prototyped.*

**What happens**: `process_queue_submit()`
(`virtio-devices/src/block.rs:245-390`) leaves its drain loop on an empty
ring without calling `enable_notification()`. `avail_event` is only
published per completion (`block.rs:~601`). The guest therefore suppresses
kicks for requests submitted while I/O is in flight; they are only picked up
by the drain that follows the next completion (`block.rs:704-715`).

This is a self-sustaining state: the completion path re-arms *before* it
drains the waiting request, so `avail_event` always lags by one slot. At QD2
the device effectively has one request in flight, and every request waits
one full device latency in the ring before submission.

Introduced by `a2438700e` ("support event idx for virtio-blk", 2024-07). The
project's performance tests use `iodepth=128`, which hides it.

**Measurements** (SSD, `direct=on`, 4 vCPU, 1 queue, 4 KiB random read):

| Workload | Host-native | HEAD | Fix |
|---|---|---|---|
| QD1 | 10.1k IOPS, p50 102 us | 8.5k, p50 116 us | 8.4k, p50 116 us |
| QD2 | 19.5k IOPS | **9.6k (49%)**, p50 204 us | **16.6k (85%)**, p50 115 us |
| 2 jobs x QD1 | - | 9.1-9.6k, p50 ~205 us | 16.2-16.4k, p50 115 us |
| 4 jobs x QD1 | - | 24.9k | 30.4k (+22%) |
| QD4 | - | 25.8k | 31.0k (+20%) |
| QD32 | 96.9k | 95k | 95k |

The p50 of ~2x device latency at HEAD is the signature: one latency waiting in
the ring, one on the device. The effect shrinks at high queue depth (frequent
completions pick up waiting requests) and on tmpfs (completions take a few
us), and it grows with backend latency (network/cloud volumes).

**Fix** (prototype): on an empty ring, call `enable_notification()` and
continue draining if it reports new entries. This is the standard pattern
(QEMU, CHV's own `vhost_user_block`). The re-check is required: without it,
a request submitted between "ring empty" and "publish `avail_event`" can hang
until the guest I/O timeout.

```diff
                 Some(c) => c,
-                None => break,
+                None => {
+                    // Re-arm guest notifications (EVENT_IDX avail_event) and
+                    // re-check to close the race with a concurrent submission.
+                    if queue
+                        .enable_notification(&*self.mem.memory())
+                        .map_err(Error::QueueEnableNotification)?
+                    {
+                        continue;
+                    }
+                    break;
+                }
```

`fio --verify=crc32c` with 4 jobs x QD2 on the patched build: 0 errors.

**Before submitting**: only count popped chains towards the `queue_size`
drain cap; consider dropping the per-completion `enable_notification()` in a
follow-up (saves a SeqCst fence per I/O); add a unit test checking
`avail_event == next_avail` after an async drain.

### B2: Backend choice and fallback are invisible

*Benefit 5, ease trivial (warn log) / small (`vm.info`), measured + code.*

- RAW images try io_uring, then Linux AIO, then sync
  (`block/src/factory.rs:128-167`). io_uring fails e.g. with
  `kernel.io_uring_disabled` or a restrictive container seccomp profile.
- The choice and any fallback are logged at **info** level only; the default
  log level is warn. The backend is not shown in `vm.info` or counters.
- The overrides `_disable_io_uring` / `_disable_aio` are underscore options
  marked "For testing use only" (`vmm/src/vm_config.rs:401-406`) and are
  neither in `--help` nor in the OpenAPI spec.
- Falling back to sync costs 10x at QD32 on the SSD (see B3), silently.

**Action**: log a fallback at warn level; optionally expose the backend in
`vm.info`.

### B3: Backend comparison (io_uring vs AIO vs sync)

*Data for the tuning document; no bug.*

Selected with `_disable_io_uring=on` (AIO) and additionally `_disable_aio=on`
(sync). 4 vCPU, HEAD.

**SSD, `direct=on`, 1 queue:**

| Workload | io_uring | AIO | sync |
|---|---|---|---|
| QD1 | 8.5k IOPS | 8.4k | 8.6k |
| QD32 | 96.2k | 93.9k | **9.7k** (p50 3.3 ms) |
| 4 jobs x QD1 | 26.1k | 27.4k | 9.2k |
| 1 MiB sequential read | 561 MB/s | 562 MB/s | 489 MB/s |

The sync backend runs `preadv`/`pwritev` inline on the queue thread
(`engine_sync.rs:46-66`), i.e. effectively QD1 per queue. With 4 queues and
4 jobs x QD1 it reaches 29.3k, so it only scales via queues.

**tmpfs, 4 vCPU:**

| Workload | io_uring | AIO | sync |
|---|---|---|---|
| QD1, 1 queue | 55-58k IOPS, p50 12-13 us | 74-80k, p50 8.6-8.8 us | 84-85k, p50 7.8-8.0 us |
| QD32, 1 queue | 408-429k | 366k | 416-419k |
| 4 jobs x QD32, 4 queues | **1.25M** (VMM ~540% CPU) | 0.94M (~300-340%) | 1.05-1.07M (~290-310%) |
| 1 MiB sequential, 4 queues | 12.1 GB/s | 8.2-8.4 GB/s | 8.3 GB/s |

On tmpfs, io_uring hands reads to io-wq kernel workers (`iou-wrk` threads
visible), which costs ~4 us per I/O at QD1 but scales best with parallel
load, at roughly 2x the host CPU.

**ext4 page cache (warm, `direct=off`), 1 queue:** io_uring 78-83k IOPS at
QD1 (p50 8.3-8.6 us), sync 74-84k (8.0-8.2 us), AIO 80-81k; at QD32
io_uring 451-490k, sync 369-414k, AIO 325-360k. So the QD1 penalty above is
specific to tmpfs, not to io_uring in general.

**Conclusion**: io_uring as default is right. AIO is a reasonable fallback;
sync is only acceptable with many queues or for tests.

### B4: Queue count and queue size

*Data for the tuning document; defaults are acceptable.*

**tmpfs, 12 vCPU, io_uring, 4 KiB random read** (IOPS):

| Workload | 1 queue | 2 | 4 | 8 | 12 |
|---|---|---|---|---|---|
| 1 job x QD32 | 401k | 426k | 411k | 393k | 413k |
| 4 jobs x QD32 | 512k | 748k | 762k | 1.09M | 1.31M |
| 12 jobs x QD32 | 284k (p50 1.17 ms, p99 4.0 ms) | 642k | 887k | 1.11M | **1.21M** (p50 289 us, p99 815 us) |
| 12 jobs x QD1 | 175k | 223k | 161k | 143k | 170k |
| VMM + io-wq CPU (12 x QD32) | 84% | 156% | 272% | 379% | 442% |

Host-native for 12 jobs x QD32 on the same file: 7.7M IOPS. At 4 vCPU with
4 jobs x QD32: 0.43-0.49M (1 queue), 1.25M (4 queues), 3.3M native.

- Queues help only if several guest threads submit I/O in parallel. A single
  submitter gains nothing; many QD1 submitters gain nothing but cost CPU.
- On the SATA SSD (~100k IOPS), 1 queue is as fast as 4 once B1 is fixed; a
  single queue thread saturates the device at 72-96% CPU.
- Under density (4 VMs x 4 vCPU, 4 jobs x QD32 each, tmpfs), 4 queues per
  disk still gave +44% in total (0.91M -> 1.31M IOPS).
- `queue_size=256` helps a single queue under deep parallel load (12 x QD32:
  284k -> 355k, +25%); 1024 adds nothing. With more queues it is irrelevant.
- Idle cost of queues: one thread per queue (a VM with 8 disk and 16 net
  queues has 32 threads instead of 18) but 0% CPU and no RSS increase.
- `num_queues` must not exceed the boot vCPU count (validation error).

### B5-B9: Code-reading findings (not measured)

| ID | Finding | Location | Benefit | Ease |
|---|---|---|---|---|
| B5 | Discard and write-zeroes run `fallocate` synchronously on the queue thread (since `d7a7d7362`, 2026-07-17) and are advertised with max sectors `u32::MAX`; a large discard stalls the whole queue. On tmpfs, write-zeroes without unmap falls back to a 64 KiB `pwrite` loop. | `block/src/formats/raw/engine_uring.rs:120-144`, `block/src/sparse.rs:62-100`, `block.rs:881-890` | 3 | medium (`IORING_OP_FALLOCATE` or helper thread, cap max sectors) |
| B6 | AIO backend: every completion goes through `complete()`, which writes the eventfd, i.e. one extra syscall and wakeup per I/O. Regression from `ce9b49d98` (2026-07-25). AIO is only the fallback. | `block/src/io/async_io/aio_data_io.rs:145`, `completion.rs:72` | 2 | trivial |
| B7 | If the guest switches the disk to writethrough, every write completion triggers a blocking `fsync` on the queue thread. | `block.rs:535` | 2 | small (async fsync) |
| B8 | Per request: linear scans over all in-flight requests (entries ~580 B due to `SmallVec<[_;32]>`, copied 3-4 times) and SeqCst atomics shared by all queue threads. Relevant only when the queue thread is CPU-bound. | `block.rs:148-166`, `:220-226`, `:458-476`, `:496-575` | 2 | small (slot array, per-queue counters) |
| B9 | `queue_affinity` is not validated: unknown queue indices are ignored silently, a failing `sched_setaffinity` is only logged. | `vmm/src/config.rs:1656-1712`, `block.rs:649-656`, `:1207` | 1 | trivial |

## virtio-net (tap)

**TL;DR**: Small-packet receive towards the guest is the main performance
gap (77% loss, 4x headroom); a trivial feature flag recovers a third of it,
the full fix is mergeable RX buffers. The rate limiter silently multiplies
with the number of queue pairs. More queue pairs help multi-flow workloads
(+37%) but not single flows or saturated hosts. Offloads must stay on.

### N1: Small packets towards the guest are dropped

*Benefit 7, ease medium-large (mergeable RX buffers); partial fix trivial
(+35%, prototyped).*

**What happens**: CHV offers neither `VIRTIO_NET_F_MRG_RXBUF` nor
`VIRTIO_RING_F_INDIRECT_DESC` (`virtio-devices/src/net.rs:575-610`;
`net_util/src/queue_pair.rs:343` always writes `num_buffers=1`). With guest
TSO negotiated, Linux uses "big packets" RX mode: each RX buffer is a chain
of 19 descriptors, so a 256-entry ring holds ~13 buffers and every 64 B frame
walks 19 descriptors. Both features were added in `58d25b3cc` and reverted
in `495943421` (2021), because mergeable RX was never implemented.

**Measurements** (4 vCPU, 1 queue pair, 64 B UDP, 1 stream):

| Config | host->guest received | Loss | TCP g2h / h2g |
|---|---|---|---|
| HEAD | 0.31 Mpps (1.36-1.40 Mpps sent) | 77-78% | 41-44 / 50 Gbit/s |
| + `INDIRECT_DESC` (prototype) | 0.41-0.44 Mpps (+35%) | 69-70% | 41-44 / 54-59 Gbit/s |
| HEAD, offloads off (small RX buffers) | **1.24 Mpps (4x)** | 5.8% | **5.7 / 9.3 Gbit/s** |
| HEAD, `queue_size=1024` | +9% | ~76% | unchanged |

guest->host 64 B UDP reaches 0.64-0.67 Mpps with 0% loss. More queue pairs
raise host->guest pps with multiple flows (8 vCPU, 4 streams: 0.24 -> 0.50
Mpps with 1 -> 4 pairs), but loss stays above 80%.

**Action**:
1. Offer `INDIRECT_DESC` (trivial; block already does it, `block.rs:823`).
2. Implement mergeable RX buffers. This needs the frame length before
   distributing a frame over multiple chains; a tap fd cannot be peeked, so
   this requires a bounce buffer (as QEMU does) or another approach. The
   offloads-off result shows the headroom: ~4x small-packet RX while keeping
   TSO throughput.

### N2: Rate limiter applies per queue pair

*Benefit 7, ease small-medium, measured.*

Each queue pair gets its own RX and TX rate limiter
(`virtio-devices/src/net.rs:966-976`), so the effective limit scales with the
number of active pairs. `docs/io_throttling.md` does not mention this.
virtio-blk shares one limiter group across all queues of a device
(`vmm/src/device_manager.rs:2716-2740`).

| Config (`bw_size=12500000,bw_refill_time=100`, i.e. 1 Gbit/s) | g2h | h2g |
|---|---|---|
| 1 queue pair, 4 streams | 1.0 Gbit/s | 1.0 Gbit/s |
| 4 queue pairs, 4 streams | **4.0 Gbit/s** | **4.0 Gbit/s** |

Operators who set a per-VM bandwidth limit and enable multi-queue get N times
the configured limit.

**Action**: share one limiter (per direction) across all queue pairs of a
device, like virtio-blk; until then, document the behavior.

### N3: Default `--net` fails on a pre-created multi-queue tap

*Benefit 5, ease small (~15 LOC), prototyped.*

`check_mq_support()` (`net_util/src/open_tap.rs:39-56`) rejects a
multi-queue tap when only one queue pair is configured
(`MultiQueueNoDeviceSupport`), so `--net tap=tap0` with defaults does not
start. The kernel requires `IFF_MULTI_QUEUE` to attach to such a tap, even for
a single queue. The prototype opens an existing multi-queue tap with that
flag and drops the check; tested: HEAD fails, the patched build boots with
1 queue pair and passes traffic.

### N4: TX loop without work budget

*Benefit 3, ease small, measured (symptom) + code.*

The TX handler processes until the ring is empty
(`net_util/src/queue_pair.rs:55-180`); RX and TX of a pair share one thread.
Host->guest ping during a guest 64 B UDP flood: avg 0.13 -> 0.21 ms, p99
0.17 -> 0.48 ms (4 vCPU, 1 pair). The flood also loads the guest, so the
share caused by the TX loop is not isolated.

**Action**: cap one TX run (vhost-net uses 256 packets / 512 KiB) and
re-queue the remaining work.

### N5: Queue pairs, queue size, MTU, offloads (tuning data)

*Data for the tuning document.*

`num_queues` counts RX + TX queues (pairs = `num_queues / 2`, default 1
pair). The guest enables all offered pairs automatically (no `ethtool -L`
needed).

**8 vCPU, TCP** (single runs, +-10%; g2h / h2g in Gbit/s):

| Pairs (`num_queues`) | 1 stream | 4 streams | 8 streams |
|---|---|---|---|
| 1 (2) | 41 / 61 | 40 / 40 | 40 / 39 |
| 2 (4) | 43 / 50 | **55 / 55** | 41 / 43 |
| 4 (8) | 44 / 54 | 52 / 44 | 45 / 39 |
| 8 (16) | 33 / 40 | 47 / 40 | 42 / 35 |

- 4 vCPU, 4 streams: 1 pair 41 / 40 Gbit/s, 4 pairs 62 / 52 Gbit/s
  (+51% / +31%).
- 12 vCPU, 12 streams: 1 pair 39 / 39 Gbit/s, 12 pairs 50 / 41-42 Gbit/s.
  The host (12 threads) is the limit.
- Density (4 VMs x 4 vCPU, 4 streams each, concurrently): 1 pair 38.5 / 31.7
  Gbit/s total, 4 pairs 33.8 / 30.2 Gbit/s. On a saturated host, extra pairs
  do not help.
- Single stream: 41-44 Gbit/s g2h, 50-61 Gbit/s h2g; host loopback reference
  72 Gbit/s (1 stream) / 184 Gbit/s (4 streams). Ping ~0.1 ms.
- `queue_size` 512 / 1024 and `mtu=9000`: no measurable gain (TSO already
  produces 64 KiB segments).
- Offloads off: TCP drops from 44 / 50 to 5.7 / 9.3 Gbit/s. Keep the
  defaults.

### N6-N7: Code-reading findings and small gaps

| ID | Finding | Location | Benefit | Ease |
|---|---|---|---|---|
| N6a | When the RX ring runs empty, the tap fd is removed from epoll and re-added on the next guest kick (2 `epoll_ctl` + extra wakeup per refill cycle; ~29k cycles/s under small-packet load). | `net_util/src/queue_pair.rs:558-570`, `virtio-devices/src/net.rs:250-277` | 2 | small |
| N6b | Notifications are re-enabled after every descriptor chain (SeqCst fence, extra guest kicks while the device is busy). | `queue_pair.rs:174-179`, `:374-379` | 2 | small |
| N6c | The MSI-X config mutex is held across the irqfd write, serializing interrupt delivery of all queue threads of a device (also applies to virtio-blk). | `virtio-devices/src/transport/pci_device.rs:921-947` | 2 | small |
| N6d | UDP segmentation offload (USO) is not offered (QUIC and similar). | `net.rs:585-604`, `net_util/src/lib.rs:155-174` | 2 | small |
| N7a | Odd `num_queues` values are accepted; `num_queues=3` silently yields 1 pair (measured). | `vmm/src/config.rs:1893-1918` | 1 | trivial |
| N7b | `mtu` is missing from the `--net` help text (it is in the OpenAPI spec). | `vmm/src/config.rs:1733-1741` | 1 | trivial |

## General / VM lifecycle

**TL;DR**: The lifecycle is in good shape: boot barely depends on the vCPU
count, shutdown takes 18-50 ms, hotplug is visible in the guest after 5-6 ms,
48 reboots ran without errors. Findings are small: an fd leak per reboot, an
HTTP 500 on quick re-add, 1 ms polling in pause/resume, some millisecond-range
costs, and three unverified correctness issues found by code reading.

### G1: One epoll fd leaks per VM creation / reboot

*Benefit 5, ease trivial, prototyped.*

`SerialManager::new()` creates a raw epoll fd (`vmm/src/serial_manager.rs:127`)
and wraps it in an `OwnedFd` only at line 215. The early `return Ok(None)`
paths (lines 167 and 170: serial `off`/`null`, or `tty` with a non-tty stdin,
e.g. daemons and CI) leak it. Regression from `4c2b2110c` (2026-03-06).

Measured: 269 -> 317 fds over 48 reboots; 80 -> 95 over 15 reboots at HEAD,
78 -> 78 with the fix (wrap in `OwnedFd` right after creation). With a soft
limit of 1024 fds, reboots would start failing after roughly 750 cycles.

### G2: Re-adding a device right after `remove-device` fails with HTTP 500

*Benefit 3, ease trivial, measured.*

Disk hotplug: `add-disk` returns after 1.2 ms, the disk is visible in the
guest after 4-6 ms; `remove-device` returns after 1 ms, but the guest ejects
the disk only after 36-46 ms. An immediate re-add with the same id failed 6
of 6 times with "Invalid identifier as it is not unique"
(`vmm/src/device_manager.rs:5615`), mapped to HTTP 500. The device tree entry
lives until the guest ejects (`device_manager.rs:5114`).

**Action**: return 409 instead of 500 and document that removal completes
asynchronously (poll `vm.info`), or offer a way to wait for completion.

### G3: Pause/resume poll in 1 ms steps

*Benefit 2, ease trivial, prototyped.*

The VMM waits for vCPU acknowledgements with `thread::sleep(1ms)`
(`vmm/src/cpu.rs:824`, `:2772-2775`, `:2800-2803`); vCPUs need only tens of
us. Raw HTTP API latency:

| vCPUs | pause HEAD -> 50 us sleep | resume HEAD -> 50 us sleep |
|---|---|---|
| 1 | 1.47 -> 0.44 ms | 1.31 -> 0.27 ms |
| 4 | 1.52 -> 0.53 ms | 1.32 -> 0.29 ms |
| 24 | 2.01 -> 0.90 ms | 1.39 -> 0.30 ms |

The prototype keeps sleeping (the code comment cites priority inversion) and
scales the timeout/warning counters so their wall-clock meaning is unchanged.

### G4: Unhelpful error when `image_type` is missing

*Benefit 2, ease trivial, measured.*

HEAD rejects disks without `image_type` (`38695ece1`, intentional) with
"Image type required for disk" (`vmm/src/config.rs:224`). The message should
name `image_type=` and the valid values.

### G5: Core scheduling costs 8-17 ms per boot

*Benefit 2, ease small, measured (medium confidence on the cause).*

Time from "Starting vCPUs" to "booted" in the CHV log: 1 vCPU 7.5 ms, 4 vCPU
8.5-10 ms, 12 vCPU 14 ms, 24 vCPU 16.6 ms; with `core_scheduling=off` 0.4 ms.
Suspected cause: the first `PR_SCHED_CORE_CREATE` flips the kernel's
`sched_core` static key (host-wide), and non-leader vCPU threads busy-spin
while waiting (`vmm/src/cpu.rs` ~1290). The effect on time-to-userspace is
within noise; the host-wide side effect per VM start/stop is the more notable
part.

### G6: Shutdown replies only after full teardown

*Benefit 2, ease medium, measured.*

`vm.shutdown` / `vmm.shutdown` reply after the `Vm` is dropped
(`vmm/src/lib.rs:2720-2730`): munmap of guest RAM, KVM VM destruction, tap
teardown, thread joins. "shutdown" -> "deleted" events: 14-17 ms at 1 GiB,
42-49 ms at 16 GiB; end-to-end shutdown 18-20 ms (1 GiB) and 45-51 ms
(16 GiB).

### G7: API head-of-line blocking

*Benefit 2, ease medium, code + partly measured.*

One HTTP thread and one VMM thread; every request blocks until done, and
`vmm.ping` also goes through the VMM thread (`vmm/src/api/http/mod.rs:381-398`).
So ping/info stall behind boot, shutdown or hotplug. micro-http caps
connections at 10. Measured: `ch-remote` costs 0.62-0.71 ms per call vs
0.04-0.08 ms for a raw HTTP request (process spawn), so control planes should
talk HTTP directly.

### G8: Prefault thread count bug (no gain on this host)

*Benefit 1-3, ease trivial, prototyped.*

`get_prefault_num_threads()` (`vmm/src/memory_manager.rs:2449`) divides by
`64 * (1 << 26)` = 4 GiB while the comment says 64 MiB, so each RAM region of
up to 4 GiB is prefaulted by one thread (since `7633d47293`, 2024). On this
dual-channel host, one thread already saturates DRAM (host test: 1 thread
25.6 GiB/s, 12 threads 22.9 GiB/s); the fixed build is neutral to slightly
slower (16 GiB: prefault 805 vs 825 ms, time-to-userspace 1738 vs 1734 ms).

**Action**: fix code or comment, but validate on a multi-channel/NUMA server
before changing the thread count.

### G9: Debug port timestamps >= 1 s are wrong

*Benefit 1, ease trivial, code.*

`devices/src/legacy/debug_port.rs:75-79` prints `elapsed.as_micros()` instead
of `subsec_micros()`, so 1.234567 s is printed as "1.1234567".

### G10: RSS grows ~16 KiB per reboot

*Benefit 1, ease unknown, measured.*

4,202,676 -> 4,203,428 KiB over 48 reboots (4 vCPU / 4 GiB with disk, net,
vsock), linear. Cause not found; low confidence that this is a real leak.

### G11: Stale `vcpu_run_interrupted` after an NMI

*Benefit 2, ease small, code (not reproduced).*

`nmi()` sets `vcpus_kick_signalled`, and the vCPU thread sets
`vcpu_run_interrupted = true` (`vmm/src/cpu.rs:1393`), which is only reset in
the pause path (`cpu.rs:1389`). `signal_vcpus()` does not reset it before
signalling, so after one NMI the next acknowledgement check
(`wait_until_signal_acknowledged()`, `cpu.rs:815-835`) passes immediately
without the vCPU having left `KVM_RUN`. A following NMI can then be lost
because `vcpus_kick_signalled` is cleared before the vCPU sees it.

### G12: `ch-remote` can panic on non-ASCII responses

*Benefit 2, ease trivial, code (not reproduced).*

`parse_http_response()` (`api_client/src/lib.rs:107`) calls
`str::from_utf8(...).unwrap()` on each 256-byte chunk. A multi-byte UTF-8
character split across two reads (e.g. a non-ASCII path in `vm.info`) panics.
**Action**: accumulate bytes and decode once.

### Healthy results

- Cold boot barely depends on vCPUs (1-24 vCPU at 1 GiB: 0.95-0.99 s to
  userspace); the VMM part is ~55 ms at 1 GiB, dominated by prefault.
- API-driven boot (`create` + `boot`) matches CLI boot.
- Shutdown (guest poweroff, API, ACPI power button): 18-25 ms at 1 GiB,
  45-59 ms at 16 GiB; 1 / 4 / 8 VMs shut down concurrently in 22 / 39 / 48 ms.
- 48 reboots (guest and API initiated) without errors; disk and network
  worked after every reboot; threads constant; hugepages not leaked.
- Hotplug: disk visible after 4-6 ms, NIC after ~6 ms, NIC removal 2-4 ms;
  no fd or thread leaks over repeated cycles.
- Many devices (4 disks x 24 queues + 4 NICs x 32 queues at 24 vCPU) add only
  ~50 ms to boot.
- Pause/resume: 280 API calls without errors.
- virtio-blk reaches the device limit at QD32 and for sequential I/O on the
  SSD; virtio-net single TCP stream at 61-75% of host loopback.

## Plan: performance tuning document

A new `docs/performance-tuning.md` (about 1-2 pages), phrased as rules rather
than absolute numbers, linking to `docs/io_throttling.md` and
`docs/performance_metrics.md`. It should describe the behavior after the bug
fixes B1, N2 and N3, or state them as caveats.

1. **Scope and measuring**: raw disk images, in-VMM virtio-net with tap;
   measure with the guest workload in mind (parallelism matters more than
   anything else).
2. **virtio-blk (raw)**
   - Backend: io_uring by default, then AIO, then sync. How to check (`-v`,
     log line), why a fallback happens (`kernel.io_uring_disabled`, container
     seccomp), and that sync means QD1 per queue.
   - `num_queues`: raise only for parallel I/O in the guest on fast backends
     (NVMe, memory); at most the vCPU count; no gain for single-threaded
     workloads or SATA-class devices; each queue costs a host thread and CPU
     under load.
   - `queue_size`: 256 when keeping one queue under deep parallel load;
     larger values bring nothing.
   - `direct`: semantics (host page cache, host memory use, flushes still
     needed for durability); not a performance knob.
3. **virtio-net (tap)**
   - `num_queues` = 2 x queue pairs; pairs <= vCPUs; the guest enables them
     automatically. 2-4 pairs for multi-flow workloads; no gain for single
     flows or saturated hosts.
   - Pre-created taps: `multi_queue` must match (until N3 is fixed).
   - Offloads: keep enabled. `queue_size` and MTU do not help TCP.
   - Rate limiter: currently per queue pair (until N2 is fixed).
4. **Summary table**: option, default, when to change, cost.

## Action items

Sorted by benefit, then ease. "Proto" = fix prototyped and A/B-tested.

| # | Action | Area | Benefit | Ease | Status |
|---|---|---|---|---|---|
| B1 | Re-arm notifications after draining the virtio-blk queue | blk | 8 | small | proto |
| N2 | Share the net rate limiter across queue pairs (or document) | net | 7 | small-medium | measured |
| N1a | Offer `INDIRECT_DESC` for virtio-net | net | 4 (+35% small-packet RX) | trivial | proto |
| N1b | Implement mergeable RX buffers | net | 7 | medium-large | ceiling measured |
| G1 | Fix epoll fd leak in `SerialManager::new()` | general | 5 | trivial | proto |
| N3 | Allow a single queue pair on a multi-queue tap | net | 5 | small | proto |
| B2 | Warn on backend fallback; show backend in `vm.info` | blk | 5 | trivial / small | measured |
| D1 | Write `docs/performance-tuning.md` | docs | 4 | small | plan above |
| B5 | Make discard/write-zeroes asynchronous, cap max sectors | blk | 3 | medium | code |
| G2 | Return 409 on re-add during pending removal; document | general | 3 | trivial | measured |
| N4 | Add a TX work budget per run | net | 3 | small | measured |
| G3 | Shorter sleep in pause/resume acknowledgement polling | general | 2 | trivial | proto |
| G4 | Name `image_type=` and valid values in the error | general | 2 | trivial | measured |
| G11 | Reset `vcpu_run_interrupted` after NMI kicks | general | 2 | small | code |
| G12 | Decode `ch-remote` responses after reading all bytes | general | 2 | trivial | code |
| B6 | Remove per-completion eventfd write in AIO backend | blk | 2 | trivial | code |
| B7 | Asynchronous fsync in writethrough mode | blk | 2 | small | code |
| B8 | Slot array and per-queue counters on the request path | blk | 2 | small | code |
| N6a-d | RX epoll churn, per-chain re-enable, MSI-X lock, USO | net | 2 each | small | code |
| G5 | Avoid busy-spin during core scheduling setup | general | 2 | small | measured |
| G6 | Reply to shutdown before freeing guest memory | general | 2 | medium | measured |
| G7 | Answer `vmm.ping` without the VMM thread | general | 2 | medium | code |
| G8 | Fix prefault thread divisor (validate on servers first) | general | 1-3 | trivial | proto (neutral here) |
| B9 | Validate `queue_affinity` | blk | 1 | trivial | code |
| N7 | Reject odd net `num_queues`; add `mtu` to `--net` help | net | 1 | trivial | measured / code |
| G9 | Fix debug port timestamp formatting | general | 1 | trivial | code |
| G10 | Investigate RSS growth across reboots | general | 1 | unknown | measured |
