# Live migration: dirty log to wire, performance analysis

Precopy, from fetching the dirty log to writing memory on the socket, plus the
receive path. Small = 8 GiB guest, large = 64-512 GiB.

- **Setup:** micro benchmarks against the real crates (`vm-migration`,
  `vm-memory` 0.18, `kvm-ioctls` 0.25, `opt-level = "s"`); macro: 2 GiB guest
  migrated between two VMMs over TCP loopback (MSS 64 KiB, so TCP results
  resemble a jumbo-frame link). Host: 4-core laptop, Intel with PML, THP
  `enabled=madvise`. 512 GiB kernel numbers are extrapolated.
- **Per iteration at 10% dirty:** kernel `KVM_GET_DIRTY_LOG` ~7 ms (8 GiB) /
  ~58 ms (64 GiB); bitmap to range table ~1.2 / ~25 ms; `partition()`
  negligible; sending dominates.
- **Kernel scaling:** 1.5 ms/GiB at 100% dirty, 4 us/GiB when clean. The guest
  pays ~260 ns per re-dirtied page (~68 ms of vCPU time per GiB).

## 1. One `write(2)` per dirty range
**TL;DR: batch with `writev`. Largest single win.**
- 262k syscalls per GiB of scattered pages, one receiver wakeup each.
- Fix: `writev` over header + table + memory, `IOV_MAX` batches, guest memory
  by raw pointer (guest writes concurrently). TLS instead stages ranges into
  one buffer, else each range becomes a TLS record.
- Gain: fragmented 2.0 to 4.8 GiB/s (UNIX), 1.3-2.1 to 3.5-4.3 (TCP); 262k
  syscalls/GiB to 262. Contiguous is already at the socket limit.

## 2. One `read(2)` per dirty range
**TL;DR: same fix on the destination.**
- Fix: `readv`, mark the bitmap per slice before reading.
- Gain: wakeups per request 31.5k to 270, destination CPU roughly halved; with
  item 1, fragmented reaches 75-90% of contiguous throughput.

## 3. Nagle stalls
**TL;DR: `TCP_NODELAY`, but only after items 1-2.**
- Request/response pattern + delayed ACK = ~40 ms per affected request.
- Without batched writes, `TCP_NODELAY` puts every 4 KiB page in its own
  segment: ~3x slower, ~2x CPU.
- Gain: size independent; idle 2 GiB downtime 85 to 4 ms. At MTU 1500 only
  payloads below ~8 KiB stall.

## 4. Range tables copied on merge
**TL;DR: reuse the largest table's allocation.**
- Merging device tables and the final leftover table reallocates and copies.
- Gain/iteration: 0.28 ms (8 GiB, 10%), 8.6 ms (64 GiB), 133 ms (512 GiB);
  partly inside downtime.

## 5. Per-range logging
**TL;DR: log a summary; buffer each record.**
- One line per range, and ~20 `write(2)` per record (unbuffered, per token).
- Gain: none at default level; with debug logging a 61k-range iteration spent
  0.55 s logging, more than transferring the same data.

## 6. `AtomicBitmap::get_and_reset()` (vm-memory)
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

## 7. THP lost when prefaulting
**TL;DR: `MADV_HUGEPAGE` before `MADV_POPULATE_WRITE`.**
- With host policy `madvise`, pages faulted in before the marking stay 4 KiB.
- Gain (2 GiB): `AnonHugePages` 0 to fully backed, prefault 0.34 to 0.10 s.
- Trade-off: on a fragmented host THP faults compact, prefault up to 2.1 s.

## 8. Destination memory not populated for precopy
**TL;DR: populate before the transfer.**
- Precopy writes every page anyway; with `prefault=off` they fault in one by
  one during the copy.
- Precopy only (postcopy needs userfaultfd, MemFD passes descriptors); skip
  file/memfd-backed regions, which may stay sparse.
- Gain (2 GiB): destination CPU 2.09 to 0.58 s, first iteration 0.9 to
  3.5 GiB/s; less with free huge pages (9.9 to 15.0 GiB/s).

## Remaining, not analysed in depth
- `KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2` + `KVM_CLEAR_DIRTY_LOG`: no ioctl-time
  win alone; `INITIALLY_SET` removes `start_dirty_log` (4-5 ms/GiB) and the
  first-pass guest tax, per-chunk clearing reduces re-sends.
- Run-length bitmap conversion: 64 GiB at 10% 25 to 12 ms, 512 GiB at 100%
  171 to 4.6 ms; slightly slower on sparse random bitmaps.
- RAM and vhost-user/VFIO tables are concatenated without dedup: pages dirty in
  both are sent twice.
- Final iteration merges leftover + fresh table; pages dirtied in between are
  in both (~2% normally).
