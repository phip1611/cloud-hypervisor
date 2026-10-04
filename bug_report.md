# Concurrency audit - bug report

- Base: `eb7425276` (main, 2026-10-03)
- Fixes: branch `concurrency-audit-fixes`, 40 commits on top of the base
- Commit hashes below refer to that branch at the time of writing.

## Scope and method

Treewide search for wrong atomic orderings, missing or wrong memory fences,
locking problems and race conditions.

1. First pass: static audit by subsystem, plus one cross-cutting pass over all
   memory shared with the guest.
2. Second pass: every finding that was not reproduced was handed to an
   independent reader whose task was to refute it and to pin down the trigger.
3. Dynamic checks on x86_64/KVM with a small Linux guest and temporary unit
   tests. Nothing was run on aarch64, riscv64 or MSHV.

## Result in short

- No wrong atomic ordering was found. All atomics are standalone flags; the
  bugs around them are stale flags and check-then-act sequences.
- No missing fence was found. The tree has no explicit `fence()`; the virtio
  queue code gets its barriers from `virtio-queue`
  (`needs_notification()`, `enable_notification()`), and every device loop
  calls them in the right order.
- The real problems are fixed-size barrier handshakes, lock-order inversions,
  lifecycle races and missing dirty tracking for host-side writes.

## How to read the entries

- Severity: critical (silent guest data corruption), high (hang, stall or
  crash reachable by a guest or in normal operation), medium (needs rare
  timing or an unusual operation), low (theoretical or benign).
- Evidence: "reproduced" means the bug was triggered in a VM or by a unit
  test before the fix. "read" means it was derived from the code and
  confirmed by a second, independent read, but not triggered.
- Every fix commit was built on its own. New unit tests are named where they
  exist.

## Fixed bugs

### 1. Live migration: host-side writes to guest memory are not tracked

KVM's dirty log only sees writes done by the guest. Writes done by the VMM or
by the host kernel through the VMM mapping must be recorded in the vm-memory
dirty bitmap by the VMM itself.

#### 1.1 virtio-net RX payload is never marked dirty

- Severity: critical. Exposure: default virtio-net on tap, every pre-copy
  migration with inbound traffic.
- Cause: RX frames are read with `readv()` on raw iovecs
  (`net_util/src/queue_pair.rs`). Only the page with `num_buffers` and the
  used ring were tracked. virtio-net does not offer mergeable RX buffers, so
  Linux guests use multi-page chains and most payload pages were missed.
- Effect: data received during pre-copy and not yet consumed at switchover
  is stale on the destination.
- Fix: `6e4f75250` net_util: Mark RX buffers dirty after reading from the tap
- Evidence: reproduced at unit level (payload page stays clean); the
  corruption itself was not reproduced end to end.
  Test: `rx_marks_all_written_pages_dirty`.

#### 1.2 virtio-blk reads are marked dirty before the I/O

- Severity: critical, timing dependent. Exposure: pre-copy migration with
  disk reads in flight; practically unreachable when the host page cache
  serves the read, likely with millisecond-latency storage or `direct=on`.
- Cause: `block/src/io/request.rs` marked the destination pages before
  submitting the read. A dirty log harvest between the mark and the write of
  the backend consumed the mark; nothing marked the pages again.
- Fix: `452c29002` block: Mark direct reads dirty on completion instead of
  at submit
- Evidence: read. Test: `direct_read_is_marked_dirty_on_completion`.

#### 1.3 vhost-user device loses dirty logging on re-activation

- Severity: medium. Exposure: vhost-user device with a backend that supports
  dirty logging, and a device reset plus re-init during pre-copy (kexec,
  driver rebind).
- Cause: activation sent the features without `LOG_ALL` and the vring
  addresses without the log flag, although logging was active.
- Fix: `f78482701` virtio-devices: vhost_user: Keep dirty logging across
  activation
- Evidence: read. Test: `activation_keeps_dirty_logging_enabled`.
- Not covered: reconnect to a restarted backend, see open issues.

### 2. Pausing virtio devices

#### 2.1 Pause waits on a barrier sized for workers that do not arrive

- Severity: high. Exposure: default configuration, routine operations.
- Cause: `VirtioCommon::pause()` waited on a `Barrier` sized for the expected
  number of workers. It never completed when
  1. a worker had exited on an error (guest corrupts a virtqueue, tap error),
  2. a worker parked before entering its epoll loop (pause right after
     activation),
  3. a just resumed worker re-parked without acknowledging, or drained the
     pause event of the next pause (resume directly followed by pause),
  4. the barrier size did not match the spawned workers (failed activation,
     vhost-user-net re-activated without control queue).
- Effect: the control thread hangs forever with the vCPUs already parked.
- Fix: `6e3fb960d` virtio-devices: Only wait for live, parked workers when
  pausing. The barrier is replaced by a count of parked workers
  (`PausedSync`); the pause event only wakes the workers and is drained by
  the pausing thread.
- Evidence: reproduced. Case 1: a guest with a corrupted avail ring, then
  pause. Case 3: back-to-back pause/resume API calls on an idle host hung
  after 99 iterations, with a 2 ms gap after 268; with the fix 20311
  iterations in 60 s. Cases 2 and 4: read.
  Tests: `pause_does_not_wait_for_exited_worker`,
  `pause_waits_for_worker_that_has_not_entered_its_loop`,
  `pause_resume_cycles_do_not_lose_the_acknowledgement`.

#### 2.2 Resizing a disk un-pauses a paused device

- Severity: medium. Exposure: `resize-disk` while the VM is paused.
- Cause: `Block::resize()` resumed the workers unconditionally, even if they
  had been paused before the call.
- Fix: `816e11cf6` virtio-devices: block: Keep a paused device paused when
  resizing
- Evidence: reproduced (worker back in `epoll_wait` while the VM state is
  `Paused`). Test: `resize_keeps_paused_device_paused`.

#### 2.3 Watchdog worker blocks on a disarmed timerfd

- Severity: medium, tiny window. Exposure: `--watchdog` and any pause.
- Cause: the timerfd was blocking. `pause()` disarms it; a worker that had
  already seen the timer event then blocked in `read()`, and the pause never
  completed.
- Fix: `90384ad11` virtio-devices: watchdog: Don't block on a disarmed timer
- Evidence: read. Test: `test_event_of_disarmed_timer_is_ignored`.

### 3. PCI and DeviceManager locking

#### 3.1 ECAM config writes drop the virtio activation barrier

- Severity: high, guest triggerable. Exposure: needs a guest that writes
  `DRIVER_OK` through the `VIRTIO_PCI_CAP_PCI_CFG` window via ECAM (the only
  config path on aarch64/riscv64 and for PCI segments above 0). Mainstream
  guests do not use that window.
- Cause: `PciConfigIo` returned the barrier of the device, `PciConfigMmio`
  dropped it. The thread activating the device then waited forever.
- Fix: `fc7d5c03f` pci: Return the device barrier from PciConfigMmio writes
- Evidence: reproduced in a VM (segment 0 fine, segment 1 left the API
  dead). Test: `config_write_returns_device_barrier`.

#### 3.2 Device tree lock held while locking devices

- Severity: high. Exposure: a guest BAR move (Linux does one on every PCI
  hot-add) coinciding with shutdown, reboot, device removal or a pre-copy
  dirty log step. Few microseconds per BAR move.
- Cause: a vCPU relocating a BAR holds the device and takes the device tree.
  The DeviceManager walked the tree and locked each device under the tree
  lock, also in paths that run with live vCPUs.
- Fix: `d1055acf0` vmm: device_manager: Don't lock devices under the device
  tree lock
- Evidence: reproduced with a guest toggling a BAR while the host requested
  reboots (hang at request 21; 150 requests pass with the fix).

#### 3.3 Config lock held while taking the DeviceManager lock

- Severity: medium. Exposure: ACPI memory hot-add (growing) while the guest
  ejects a PCI device.
- Cause: `Vm::resize()` kept the VM config locked across DeviceManager
  calls; the eject path takes the two locks in the opposite order.
- Fix: `642cdf15c` vmm: vm: Don't hold the config lock while resizing memory
- Evidence: read.

### 4. vCPU lifecycle

#### 4.1 NMI kick acknowledgement is never cleared

- Severity: medium. Exposure: `vm.nmi`.
- Cause: the vCPU acknowledged a kick through `vcpu_run_interrupted` and
  kept running, so the flag stayed set.
- Effect: later NMI requests returned success without injecting anything
  until a pause/resume cycle; the first NMI was injected several times per
  vCPU; the stale flag disabled the signal retry of later pause/shutdown
  requests.
- Fix: `2a2e08837` vmm: cpu: Acknowledge a vCPU kick only once, and
  `a27fa9344` (doc comment that claimed the signal handler forces an exit).
- Evidence: reproduced. With the fix, 100 requests give exactly 100 NMIs
  per vCPU.

#### 4.2 Pause waits forever for a vCPU thread that left its loop

- Severity: medium on aarch64/riscv64, where every guest reboot or poweroff
  ends the vCPU thread; on x86_64 only after a triple fault or run error.
- Cause: such a thread never reports the paused state but still counts as
  active until it is joined.
- Fix: `44848eb68` vmm: cpu: Don't wait for vCPU threads that left their
  run loop
- Evidence: read; not reproducible on x86_64 in the normal reboot path.
  Test: `exited_thread_acknowledges_signals`.

#### 4.3 A failed pause is not rolled back

- Severity: medium. Exposure: a pause racing a guest reboot on x86_64, a
  pause during virtio driver init, CPU hotplug register access.
- Cause: on an acknowledgement timeout `pause()` returned an error but left
  the pause request set. The vCPUs parked while the VM was still considered
  running, and resume was rejected.
- Fix: `0ac440e33` vmm: cpu: Roll back a failed vCPU pause
- Evidence: reproduced (65 failed pauses in a 100 s stress run against a
  rebooting guest, each followed by a rejected resume).

#### 4.4 `vcpu_states` held while waiting for an acknowledgement

- Severity: medium. Exposure: x86_64, a vCPU in the ACPI CPU hotplug
  handler when a pause or shutdown starts.
- Cause: `signal_vcpus()` held the mutex for the whole wait; the vCPU needed
  it to finish its exit and acknowledge.
- Fix: `87cf80fcd` vmm: cpu: Release vcpu_states while waiting for a signal
  ACK
- Evidence: read.

#### 4.5 Failed vCPU thread setup skips the start barrier

- Severity: medium. Exposure: affinity to a host CPU that does not exist or
  is outside the cpuset, core scheduling or seccomp failure.
- Effect: boot (or vCPU hot-add) hung, SIGTERM was not served.
- Fix: `df1d0b7b8` vmm: cpu: Reach the start barrier when the vCPU thread
  setup fails. The VMM now exits like on a vCPU thread panic.
- Evidence: reproduced with `--cpus boot=2,affinity=[0@[1000]]`.

#### 4.6 Core scheduling leader can be hot-removed

- Severity: low. Exposure: SMT host, a vCPU other than 0 won the election
  at boot, resize down and up again.
- Fix: `e49db4426` vmm: cpu: Let vCPU 0 own the core scheduling cookie
- Evidence: read; fix checked in a VM (boot and hot-added vCPUs share one
  cookie).

#### 4.7 vCPU removal joins a thread parked for a pause

- Severity: medium, microsecond window. Exposure: x86_64 vCPU hot-unplug
  racing a pause.
- Fix: `05d5400a6` vmm: cpu: Don't join a parked vCPU thread on vCPU removal
- Evidence: read.

#### 4.8 `resume()` hangs when a vCPU pauses itself again

- Severity: high for debugging, `guest_debug` builds only. gdb single-step
  should hang on nearly every step.
- Fix: `ee37935bd` vmm: cpu: Don't wait in resume() for a vCPU that paused
  itself again
- Evidence: read; builds with `guest_debug`, not run under gdb.

### 5. VMM control loop and consoles

#### 5.1 Stale guest reset and exit events terminate the VMM

- Severity: medium. Exposure: API reboot, shutdown or delete coinciding with
  a guest-initiated reboot or poweroff.
- Cause: `vm_reboot()` drains the reset event, so a second handler read
  failed; a reset for a VM that is already gone failed as well. Both errors
  left the control loop. A stale guest exit could shut down a rebooted VM.
- Fix: `f4dc8e6b0` vmm: Ignore stale guest reset and exit events
- Evidence: read; not reproduced in a 90 s stress run.

#### 5.2 Serial manager dies on EAGAIN

- Severity: low impact, but hit in everyday use. Exposure: `--serial tty`
  with the console left at its default (`tty`).
- Cause: two threads read the same non-blocking stdin; the serial manager
  treated EAGAIN as fatal and exited silently.
- Fix: `1252677f2` vmm: serial_manager: Tolerate EAGAIN when reading from
  the tty
- Evidence: reproduced (thread gone after 11 keystrokes).

#### 5.3 API socket lock can end up on an unlinked file

- Severity: low. Exposure: starting a VMM on a socket path while the
  previous owner is exiting.
- Fix: `3e1587f8a` vmm: locked_unix_listener: Don't lock an unlinked lock
  file, `267ee3022` main: Don't remove the API socket after its lock was
  released
- Evidence: read; stress test
  `test_lock_is_not_held_on_an_unlinked_lock_file`.

### 6. Block and rate limiter

#### 6.1 Rate limiter arms a zero-duration timer

- Severity: high for affected configurations. Exposure: a bandwidth bucket
  slightly smaller than a guest request, hit at a full bucket.
- Cause: the refill delay was truncated to 0 ms, which disarms the timerfd
  while the limiter is marked blocked. All queues behind the limiter stall
  until the VM is recreated.
- Fix: `2f0b84493` rate_limiter: Never arm a zero-duration timer on
  overconsumption
- Evidence: reproduced by unit test and in a VM
  (`bw_size=512000,bw_refill_time=100`, first 504 KiB writeback after idle).
  Test: `test_rate_limiter_small_overconsumption_unblocks`.

#### 6.2 Rate limiter group thread panics on a re-armed timer

- Severity: low. Fix: `b5d8c79d7` rate_limiter: Don't panic the group
  thread on a re-armed timer. Evidence: read.

#### 6.3 Completion errors drop a request without returning it

- Severity: medium. Exposure: write-through mode and a failing flush on
  qcow2, VHDX, VMDK or the raw sync backend.
- Fix: `4d8180a0f` virtio-devices: block: Fail requests on completion errors
- Evidence: read. Test: `fsync_failure_completes_requests_with_ioerr`.

#### 6.4 Unserialized partial block writes under O_DIRECT

- Severity: medium. Exposure: qcow2, `direct=on`, more than one queue, host
  storage with a direct I/O alignment above 512 bytes.
- Cause: a sub-block write is a read-modify-write of the whole block; two
  queue workers could overwrite each other's sectors.
- Fix: `16a8a01ca` block: Serialize partial block writes of an AlignedFile
- Evidence: read. Test:
  `partial_block_write_keeps_concurrent_neighbor_writes`.

#### 6.5 Rust references to live guest memory

- Severity: low (undefined behaviour without an observed effect).
- Fix: `c3ab670f5` block: Don't create references to guest memory in
  AlignedFile

### 7. vhost-user

#### 7.1 Server-mode reconnect ignores the kill event

- Severity: high. Exposure: `vhost_mode=server`, backend gone, then guest
  reset, VM shutdown, reboot or device removal.
- Cause: the worker blocked in `accept()`; the thread joining it waited
  until some backend connected.
- Fix: `91308326d` virtio-devices: vhost_user: Abort server-mode connect on
  kill_evt
- Evidence: read. Tests: `server_connect_aborts_on_kill_evt`,
  `server_connect_accepts_backend`.

#### 7.2 Generic vhost-user config access panics

- Severity: medium. Exposure: guest config access while the backend is
  down or rejects the request.
- Fix: `6d931fd32` virtio-devices: vhost_user: Don't panic on generic
  config errors
- Evidence: read. Test: `generic_config_access_survives_lost_backend`.

#### 7.3 Memory update is sent to a disconnected backend

- Severity: low. The log said "skipping" but the code did not return; the
  error aborted the memory resize of the VM.
- Fix: `657003cad` virtio-devices: vhost_user: Skip memory updates when
  disconnected. Test: `add_memory_region_skips_disconnected_device`.

#### 7.4 Lock order in vhost-user-blk `state()`

- Severity: low, latent. Fix: `4d5cc53af` virtio-devices: vhost_user: blk:
  Fix lock order in state()

### 8. virtio-pci transport

These three need a concurrent rare event or a guest that writes the device
status from two vCPUs; a mainstream guest will not hit them.

- 8.1 NEEDS_RESET set by a worker raced with the status write of the
  driver; a reset could be swallowed. Fix: `02ee76ce6` virtio-devices: pci:
  Fix races between status writes and NEEDS_RESET.
- 8.2 `config_generation` could stay unchanged across a torn config read.
  Fix: `ccd501dc3` virtio-devices: pci: Bump config_generation on
  generation reads.
- 8.3 A reset while an activation was pending left a stale activated
  device. Fix: `ec3a7d632` virtio-devices: pci: Don't activate a device
  that was reset meanwhile.

All three have unit tests.

### 9. Other

#### 9.1 MSHV SEV-SNP host access bitmap

- Severity: high for `mshv` + `sev_snp`. A page revoked by the guest stayed
  marked as host accessible (wrong units, and updates on a private copy), so
  a later device access did not re-acquire it.
- Fix: `f51b71df1` hypervisor: mshv: Fix the host access bitmap on page
  revocation
- Evidence: read; type-checked with the features, not run.

#### 9.2 TPM responses get out of sync after a receive timeout

- Severity: medium. Exposure: `--tpm` and a swtpm command slower than
  100 ms. Every later command received the response of its predecessor.
- Fix: `fd2c0a343` tpm: Keep requests and responses paired after a receive
  timeout
- Evidence: read. Test: `test_late_response_is_not_taken_for_next_command`.

#### 9.3 virtio-mem DMA mapping handler removal

- Severity: low. Fix: `5f81f0b7f` virtio-devices: mem: Hold config while
  removing a DMA mapping handler. Evidence: read.

#### 9.4 TLS migration receive

- Severity: medium. Exposure: TLS migration and a third party that can
  reach the port. A silent peer blocked the accept thread and with it the
  receiving VMM; a failed handshake aborted the migration.
- Fix: `d892c93bf` vmm: migration: Keep accepting after a failed TLS
  handshake, `c8428b079` vmm: migration: Bound the TLS handshake on the
  receive side
- Evidence: read; checked manually with temporary certificates, no test
  committed.

#### 9.5 Tracer

- Severity: low, `tracing` builds only. Unsynchronized global state, a
  second boot panicked, restore paths panicked.
- Fix: `e736d19c3` tracer: Synchronize the tracer state and allow
  restarting it

## Open issues

### Real, not fixed

- Guest reset or shutdown during a live migration makes the control loop
  return an error and the VMM exit. This is the in-tree TODO in
  `vmm/src/lib.rs` and is left to the separate lifecycle series.
- UFFD handler error path (`vmm/src/memory_manager.rs`): on a source error
  the userfaultfd is closed before the VM is stopped, so blocked faults
  resolve with zero pages and the guest runs on them for some milliseconds.
  On-demand restore and postcopy only.
- vhost-user `disconnected` latch is never cleared after a successful
  reconnect. Later pause/resume skip the device and a guest reset leaves it
  dead until the VM is recreated.
- vhost-user dirty logging across a backend reconnect: the new handle has
  no log region, so the migration aborts with an error. No silent loss, but
  migration does not survive a backend restart.
- qcow2: host offsets are used after the metadata lock was dropped. A
  whole-cluster discard plus flush plus allocation on other queues can reuse
  the cluster under an in-flight write. Not reachable by a normal
  filesystem workload.
- `eject_device` versus a concurrent BAR move of the same device leaks an
  MMIO range; `move_bar` has no rollback on its late error paths. Needs a
  guest that works against itself.
- gdb: stop events of several vCPUs are summed in one eventfd, and
  breakpoints are only programmed on vCPU 0. `guest_debug` only.
- Pause runs KVM_RUN once with `immediate_exit` set. For multi-fragment or
  rep-string MMIO the vCPU can park with a completion pending. No visible
  effect was found.

### Limits of the fixes

- A worker that hits an error still exits. The device stays dead until the
  guest resets it; only the hang of the next pause is gone.
- On x86_64 a pause that races a guest reboot still fails after the 1 s
  acknowledgement timeout, because the vCPU spins in the reset device. It
  is rolled back now. The spin loops were left untouched on purpose.
- A failed vCPU thread setup ends the VMM with exit code 0. Returning an
  error from `activate_vcpus()` would be cleaner.
- `vm.nmi` on a paused VM returns success without injecting an NMI.
- virtio-pci: a reset between the status re-check of the activator and the
  activation itself is still possible. Devices that change their config
  without the device lock can still produce a torn read.
- MSHV: enlarging the bitmap can lose a concurrent in-place update, and
  setting a bit after the acquire ioctl is not atomic with a release.
- `AlignedFile`: the lock is only shared between clones of one file, and an
  aligned write overlapping a partial one is not covered.
- TLS: the handshake timeout applies per read and write, not in total.
- TPM: draining assumes one `recv()` returns one whole response.
- Tracer: restore and receive-migration still do not start it.

### Reported, but not bugs

- VFIO DMA map uses a host pointer after the region lock was dropped:
  refuted, the virtio-iommu mapping mutex covers the whole call.
- Logger mutex poisoning, a panic in a migration thread, and the submit
  loop cap of virtio-blk: the mechanisms exist, but no reachable trigger
  was found.

### Noted in passing, not verified

- `Block::resize()` assigns to `self.state().disk_nsectors`, which is a
  temporary.
- The raw io_uring and AIO backends ignore fsync errors in write-through
  mode.
- The dirty range table assumes 4 KiB pages (`vmm/src/memory_manager.rs`).
- A serial socket EOF queues an empty input, which raises a spurious RX.
- `eject_device` frees the device id before checking that a device exists.

### Verification gaps

- aarch64, riscv64 and MSHV were not run. Entry 4.2 is the most relevant
  one for aarch64.
- The migration corruptions (1.1, 1.2) were shown at unit level only.
- Tap based scenarios need root and were not run, including a net worker
  that exits on a tap error.
- Not covered by the audit: the vsock connection state machine, VHDX, VMDK
  and VHD internals, the x86 instruction emulator, the postcopy
  destination, vDPA and VFIO dirty logging, TDX and SEV paths.
