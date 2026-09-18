# Live migration: performance analysis

Two parts: the memory transfer, from fetching the dirty log to writing memory
on the socket and reading it back, and the downtime, from pausing the VM until
the destination resumes it.

- **Setup:** micro benchmarks against the real crates (`vm-migration`,
  `vm-memory` 0.18, `kvm-ioctls` 0.25) and against KVM itself for the vCPU
  state. Macro numbers where given come from a 2 GiB guest migrated between
  two VMMs over TCP loopback.
- **Hosts:** the memory-transfer numbers of items 1-5 were taken on a 4-core
  Intel laptop with PML; everything measured for this branch was re-taken on a
  16-core AMD Ryzen 7 7840U, 30 GiB, ext4 on NVMe, THP `enabled=madvise`,
  `powersave` governor and about 10% background load. Absolute values carry
  that noise; the old/new ratios were stable across repetitions.
- **Links:** local benchmarks use a UNIX socket pair and TCP loopback. The
  link-dependent numbers use a veth pair between two network namespaces with
  `netem` delay, which gives about 1.4 GiB/s at 1500 byte MTU, roughly a
  10 GbE link. "RTT 1ms" and "RTT 5ms" below are that link with 0.5ms and
  2.5ms of delay per direction.
- **Scope:** items 4 and 9 to 12 are measured but not part of the series;
  they are kept here because the measurement answers the question.
- **Per iteration at 10% dirty:** kernel `KVM_GET_DIRTY_LOG` ~7 ms (8 GiB) /
  ~58 ms (64 GiB); bitmap to range table ~1.2 / ~25 ms; `partition()`
  negligible; sending dominates.
  - TODO: remeasure the bitmap to range table numbers, because upstream now
    fills one table instead of merging per-slot tables, which made that step
    2-3x faster.
- **Kernel scaling:** 1.5 ms/GiB at 100% dirty, 4 us/GiB when clean. The guest
  pays ~260 ns per re-dirtied page (~68 ms of vCPU time per GiB).

## Memory transfer

### 1. One `write(2)` per dirty range
**TL;DR: batch with `writev`. Largest single win.**
- 262k syscalls per GiB of scattered pages, one receiver wakeup each.
- Fix: `writev` over header + table + memory, `IOV_MAX` batches, guest memory
  by raw pointer (the guest writes concurrently).
- Syscalls per GiB of single-page ranges: 262176 to 262.
- Throughput and CPU per 512 MiB, by mean run length (16 cores, both ends on
  the same host, so the CPU column is sender plus receiver):

  | run | UNIX old | UNIX new | TCP old | TCP new | CPU old | CPU new |
  |----:|---------:|---------:|--------:|--------:|--------:|--------:|
  | 4 KiB | 2.91 | 4.20 | 0.77 | 3.55 | 0.97 s | 0.24 s |
  | 8 KiB | 3.75 | 4.48 | 1.31 | 4.15 | 0.58 s | 0.20 s |
  | 16 KiB | 4.59 | 5.40 | 2.04 | 4.74 | 0.36 s | 0.19 s |
  | 32 KiB | 5.21 | 5.38 | 2.93 | 5.46 | 0.25 s | 0.17 s |
  | 64 KiB | 5.53 | 5.49 | 3.10 | 5.00 | 0.24 s | 0.18 s |
  | 256 KiB | 5.69 | 5.89 | 4.25 | 5.39 | 0.19 s | 0.16 s |
  | 1 MiB | 6.05 | 6.24 | 5.59 | 5.56 | 0.16 s | 0.16 s |
  | contiguous | 5.90 | 5.62 | 5.20 | 5.21 | 0.17 s | 0.16 s |

  (GiB/s; the CPU columns are the TCP case.) The win is everything below a
  64 KiB mean run; from 1 MiB on the two are the same, and contiguous
  transfers were already at the socket limit.
- Over the 10 GbE-like link the same profile becomes link-bound rather than
  syscall-bound: at RTT 1ms, 4 KiB runs go from 0.26 to 1.45 GiB/s and the
  sender's CPU from 0.22 to 0.03 s per 64 MiB; at RTT 5ms from 0.14 to
  0.43 GiB/s.
- A staging buffer was measured as a simpler alternative, copying the ranges
  into one buffer instead of handing the kernel an iovec array. It is a tie
  for single-page ranges, and on a UNIX socket even slightly ahead, because
  16384 single-page iovecs cost the kernel more than one memcpy. But for runs
  of 16 to 64 KiB over TCP it is 25-35% behind at a 256 KiB buffer and still
  10% behind at 1 MiB, and the optimum size differs per transport (UNIX peaks
  at 128 KiB, TCP keeps improving to 1 MiB). On the netem link both saturate
  it. Vectored I/O was kept for the mid-size runs; the buffer would have been
  about 60 lines less code.

### 2. One `read(2)` per dirty range
**TL;DR: same fix on the destination, and the same code.**
- The receiving side had the same problem, and one wakeup per range on top.
- Fix: `readv`, mark the bitmap per slice before reading.
- The two directions are the same problem, so one commit covers both: the
  batch, the retry loop for a partial transfer and the walk over the range
  table are shared, and only the guard type, the permission and the syscall
  differ.
- The throughput table above is end to end, so it contains both fixes. The
  receiver's share shows in the syscall count: 16385 reads per 64 MiB chunk of
  single-page ranges become 17.
- Gain: wakeups per request 31.5k to 270, destination CPU roughly halved; with
  item 1, fragmented reaches 75-90% of contiguous throughput.

### 3. Nagle stalls
**TL;DR: `TCP_NODELAY`, but only after items 1-2.**
- A request that is written with more than one `write(2)` and then waits for a
  response hits Nagle plus the peer's delayed acknowledgement. Measured as a
  request/response loop over the netem link, per round trip:

  | request payload | one write | two writes | two writes + NODELAY |
  |----------------:|----------:|-----------:|---------------------:|
  | 64 B (RTT 0.05ms) | 0.08 ms | 41.1 ms | 0.10 ms |
  | 1 KiB (RTT 1ms) | 1.07 ms | 43.2 ms | 1.07 ms |
  | 8 KiB (RTT 1ms) | 1.06 ms | 2.14 ms | 1.08 ms |
  | 1 KiB (RTT 5ms) | 5.26 ms | 51.4 ms | 5.22 ms |
  | 64 KiB (RTT 5ms) | 5.23 ms | 10.5 ms | 5.26 ms |

  The stall is ~41 ms and does not depend on the RTT. Above one MSS it turns
  into one extra RTT instead, because the payload leaves as full segments and
  only the tail waits for an acknowledgement.
- `TCP_NODELAY` without items 1-2 makes the bulk transfer worse, as every page
  becomes its own segment: 4 KiB runs at RTT 1ms drop from 0.36 to
  0.27 GiB/s and the sender's CPU rises from 0.02 to 0.10 s per 32 MiB. With
  vectored writes the option costs nothing on the bulk path (1.25 vs
  1.36 GiB/s, within noise) and removes the stalls from the request/response
  steps.
- Gain: size independent; idle 2 GiB downtime 85 to 4 ms.

### 4. `AtomicBitmap::get_and_reset()` (vm-memory)
**TL;DR: load before swap; the win for idle or huge VMs.**
- `fetch_and(0)` on every word: cost follows RAM size, not the dirty set
  (~4.3 ns/word).
- Fix: swap only non-zero words; a bit set in between is either returned by the
  swap or reported next iteration.
- Gain: 5-19x - 140 us to ~7 us (8 GiB), 1.2 to 0.2 ms (64 GiB), 10 to 1.3 ms
  (512 GiB) per call; 70-80% of userspace dirty-log time when sparse, and two
  calls run while the VM is paused.
- Related: `mark_dirty()` is per page; word-at-a-time is 13-70x faster
  (matters for device I/O, not migration).
- TODO: remeasure the share of userspace dirty-log time, because the rest of
  that time shrank with upstream's faster table construction, so 70-80% is now
  too low.

### 5. Destination memory not populated for precopy
**TL;DR: populate before the transfer.**
- Precopy writes every page anyway; with `prefault=off` they fault in one by
  one during the copy.
- Precopy only (postcopy needs userfaultfd, MemFD passes descriptors); skip
  file/memfd-backed regions, which may stay sparse.
- Populating with `MADV_POPULATE_WRITE` over all cores and then copying,
  against copying into an empty mapping:

  | region | THP | fault while copying | populate | copy | total |
  |-------:|:---:|--------------------:|---------:|-----:|------:|
  | 256 MiB | no | 148 ms | 92 ms | 33 ms | 125 ms |
  | 256 MiB | yes | 32 ms | 15 ms | 18 ms | 33 ms |
  | 1 GiB | no | 344 ms | 200 ms | 72 ms | 272 ms |
  | 1 GiB | yes | 482 ms | 76 ms | 79 ms | 155 ms |
  | 2 GiB | yes | 936 ms | 303 ms | 194 ms | 497 ms |

  The win comes from populating in parallel instead of faulting in the receive
  thread, so it shrinks when many receive connections already spread the
  faults. With THP it is larger, because a fault that has to compact a huge
  page stalls the copy; this host is fragmented, which is why the 1 and 2 GiB
  THP rows are the worst case rather than the typical one.
- Gain (2 GiB, macro): destination CPU 2.09 to 0.58 s, first iteration 0.9 to
  3.5 GiB/s; less with free huge pages (9.9 to 15.0 GiB/s).

## Downtime

Everything below runs with the VM stopped. The steps are, in order: pause the
vCPUs, send the final memory delta, capture and serialize the VM state, hand
it over, let the destination restore and resume, acknowledge.

### 6. Polling for vCPU acknowledgements
**TL;DR: one condition variable removes a fixed ~2 ms.**
- Both waits slept 1 ms between checks while the vCPU threads acknowledge
  within microseconds. Modelled with parked threads that acknowledge after a
  given exit latency:

  | vCPUs | exit | pause poll | pause condvar | resume poll | resume condvar |
  |------:|-----:|-----------:|--------------:|------------:|---------------:|
  | 1 | 0 us | 1.12 ms | 0.09 ms | 1.10 ms | 0.08 ms |
  | 8 | 20 us | 1.06 ms | 0.04 ms | 1.06 ms | 0.02 ms |
  | 32 | 100 us | 1.10 ms | 0.45 ms | 1.10 ms | 0.04 ms |
  | 128 | 20 us | 1.27 ms | 0.34 ms | 1.13 ms | 0.11 ms |
  | 200 | 100 us | 3.85 ms | 2.64 ms | 1.27 ms | 0.17 ms |

- Gain: about 1.0 ms on the source's pause and 1.0 ms on the destination's
  resume, independent of guest size. Past ~128 vCPUs the exits themselves
  dominate and both variants converge.

### 7. The VM state is sent uncompressed
**TL;DR: 3.3 MB to 14 KB for 200 vCPUs, for about 1ms of CPU.**
- The state is JSON and largely the same text over and over. Per vCPU it is
  16.5 KB: CPUID 5.7 KB, MSRs 3.6 KB, xsave 2.1 KB, lapic 2.1 KB, fpu 0.9 KB,
  xcrs 0.8 KB, sregs 0.7 KB, the rest 0.6 KB.
- Three ways to exploit that were measured, for 200 vCPUs, both for an idle
  guest and for one whose vCPUs carry different xsave areas and MSR values:

  | | idle | running |
  |---|-----:|--------:|
  | plain | 3300 KB | 3766 KB |
  | move the common CPUID and the MSR indices to VM level | 1605 KB | 2070 KB |
  | drop per vCPU what the first vCPU carries identically | 191 KB | 1144 KB |
  | **zstd level 1** | **14.5 KB** | **157 KB** |
  | zstd level 1 after the VM-level split | 17.1 KB | 145 KB |
  | zstd level 1 after dropping what is identical | 25.0 KB | 310 KB |

- Compression wins in both regimes and is the only one that does not depend
  on the vCPUs being similar. Removing the repetition by hand *before*
  compressing makes the result worse, because it removes what the compressor
  would have exploited.
- Cost at 200 vCPUs: 0.90 ms to compress, 0.31 ms to decompress. Level 9
  reaches 11.7 KB but takes 3.9 ms, so the fastest level is the right one.
- It covers the device states too, which none of the hand-written schemes did.
- A sender that does not compress is still accepted: a zstd frame starts with
  its own magic, plain JSON with `{`.

### 8. vCPU state captured and serialized one at a time
**TL;DR: threads; 25ms to 4ms at 200 vCPUs, 50ms to 8ms at 400.**
- A vCPU state here is 72 CPUID entries, 132 MSRs and 16.5 KB of JSON. The
  work per vCPU is independent, and all of it is downtime.
- Capturing the states through ioctls, serial against spread over the cores:
  2.36 ms against 0.85 ms for 200 vCPUs, 0.11 against 0.16 ms for eight.
- Serializing and parsing them, serial against spread over the cores: 11.1
  against 1.4 ms on the source and 11.8 against 1.6 ms on the destination,
  for 200 vCPUs. Compression (item 7) is a single step on top of that and is
  not spread over threads, which at 0.9 ms it does not need to be.
- Below 8 vCPUs the thread pool costs about as much as it saves; the two
  `par_map` calls of a snapshot are roughly 0.15 ms each on this host.
- What neither version parallelizes stays: creating the vCPUs and applying
  their state on the destination is 5.3 + 6.0 ms at 200 vCPUs, 0.5 + 0.3 ms at
  8. That is the floor of the handover.

### 9. The completion request waits for the restore (not pursued)
**TL;DR: one RTT, which is nothing on a LAN and 5.2ms at 5ms RTT.**
- Sequential against pipelined state handover, with the destination taking the
  stated time to restore:

  | state | restore | RTT 0.05ms | RTT 1ms | RTT 5ms |
  |------:|--------:|-----------:|--------:|--------:|
  | 18 KB | 0 ms | 0.76 / 0.68 | 2.72 / 1.68 | 11.16 / 5.96 |
  | 150 KB | 2 ms | 2.62 / 2.41 | 4.49 / 3.32 | 13.25 / 7.98 |
  | 1.6 MB | 12 ms | 13.14 / 13.26 | 14.99 / 13.84 | 24.79 / 19.83 |
  | 3.6 MB | 24 ms | 26.29 / 25.96 | 27.82 / 26.91 | 37.79 / 32.32 |

  (ms, sequential / pipelined.) The saving is one RTT everywhere: below noise
  on loopback, 1.1 ms on a LAN-like link, 5.2 ms at 5 ms RTT. Sending the two
  requests back to back also couples their error handling, which is not worth
  a round trip on a local link, so this was dropped from the series.

### 10. Disk images flushed while the VM is stopped (not pursued)
**TL;DR: up to ~120 ms, but only for qcow2, which is not used here.**
- Pausing a block device flushes its format metadata, and for qcow2 that is
  `sync_all()` on the image, so everything the guest dirtied is written back
  during the downtime. Flush cost by number of images and dirty bytes each,
  plus what a second flush costs after re-dirtying a sixteenth:

  | disks | dirty each | flush | second flush |
  |------:|-----------:|------:|-------------:|
  | 1 | 16 MiB | 10.3 ms | 1.8 ms |
  | 4 | 16 MiB | 36.5 ms | 4.9 ms |
  | 16 | 16 MiB | 135.0 ms | 15.1 ms |
  | 16 | 64 MiB | 1027.3 ms | 45.8 ms |

- Flushing once precopy converged would leave the pause only the second
  flush, so the gain is the difference: 8 ms for a single image, 120 ms for
  sixteen. Raw images are unaffected, as flushing their metadata is a no-op,
  so this is worth nothing for a raw-image deployment and was dropped from
  the series.

### 11. Memory send workers joined during the downtime (not pursued)
**TL;DR: 0.1-0.2 ms, too little to carry a commit.**
- Joining workers that already finished: 0.115 ms for one connection,
  0.157 ms for four, 0.204 ms for sixteen. Keeping the pool alive until the
  destination acknowledged would take that out of the downtime, at the price
  of wrapping the whole paused phase in a closure. Dropped.

### 12. Downtime accounting and budget (not pursued)
- Neither is a gain. Reporting the pause as its own step and printing
  hundredths of a millisecond makes the breakdown add up, which it did not:
  for an idle guest about 30% of the downtime was unexplained.
- Subtracting an estimate of the state handover from the convergence budget
  turned out to be badly calibrated: the estimate of 400 us per vCPU is about
  4.6x the 86 us the handover measures per vCPU here (19 us capture and
  serialize, under 1 us on the wire, 8 us parse, 27 us create, 30 us apply). At 200
  vCPUs it would reserve 81.5 ms of the 300 ms default, so precopy would
  iterate longer than it has to for no reason. Deriving the factors from a
  measured handover would fix it; with the default downtime the problem it
  solves does not arise, so both were dropped.

## What this is worth for a real migration

Composed from the numbers above, not measured end to end. The memory delta
assumes single-page ranges, the worst case that matters; with 1 MiB runs
items 1-3 contribute nothing. The LAN columns use the local TCP socket pair,
the remote column the netem link at 5 ms RTT.

| | small: 2 vCPUs, 8 MiB delta, LAN | large: 200 vCPUs, 64 MiB delta, LAN | remote: 8 vCPUs, 16 MiB delta, RTT 5ms |
|---|---:|---:|---:|
| vCPU acknowledgements (6) | -1.9 ms | -1.9 ms | -1.9 ms |
| vCPU state (8) | ~0 | -20.9 ms | -0.4 ms |
| VM state on the wire (7) | ~0 | -2.9 ms | -0.1 ms |
| Nagle stall (3) | -41 ms | -41 ms | -51 ms |
| final memory delta (1, 2) | 10.1 -> 2.1 ms | 81 -> 17 ms | 112 -> 36 ms |
| **downtime** | **54 -> 3 ms** | **167 -> 35 ms** | **167 -> 38 ms** |

The floor of the large case is the destination: 11.3 ms to create 200 vCPUs
and apply their state, which this series does not touch. It is contained in
both columns.

Total migration time is a separate matter. It is dominated by the iterations
while the VM runs, so the 3-4.5x on fragmented transfers (items 1 and 2) and
the populated destination (item 5) decide it; for a guest that dirties memory
faster than the old code could send it, they decide whether precopy converges
at all.

## Remaining, not analysed in depth
- `KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2` + `KVM_CLEAR_DIRTY_LOG`: no ioctl-time
  win alone; `INITIALLY_SET` removes `start_dirty_log` (4-5 ms/GiB) and the
  first-pass guest tax, per-chunk clearing reduces re-sends.
- Run-length bitmap conversion: 64 GiB at 10% 25 to 12 ms, 512 GiB at 100%
  171 to 4.6 ms; slightly slower on sparse random bitmaps.
  - TODO: remeasure, because the 25 and 171 ms baselines predate upstream's
    single-table construction.
- RAM and vhost-user/VFIO tables are concatenated without dedup: pages dirty in
  both are sent twice.
- The VM state is JSON inside JSON: every per-component state is serialized to
  a string and that string is escaped again by the outer pass. That roughly
  doubles the bytes and the parsing work. Compression hides most of the cost
  on the wire but not the CPU: 11 ms to serialize 200 vCPU states, 12 ms to
  parse them. A binary encoding would remove both, at the price of
  compatibility.
