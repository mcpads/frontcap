# Shared renderer-clock commit kernel

A matching kernel is compiled into Chromium; the portable program itself is not linked into the
engine or adapter. Passing this program does not resolve the
four renderer-progress defects or qualify browser cookie/worker behavior.

Run on a POSIX host with a C++20 compiler:

```sh
sh spikes/shared-renderer-clock/run.sh
```

The runner compiles with warnings denied into a temporary directory and removes its own binary. The probe uses real `fork` processes and shared mappings. Parent and child watchdogs bound failed
handshakes. The program runs without an installed browser.

## Contract exercised

- The authority alone publishes time, bounded by a caller-authorized endpoint and every live
  participant's earliest next instant. Empty membership never permits advancement.
- Registration begins held at the current instant. Posting acquires a hold before work receives a
  timestamp or enters a runnable queue. An unchanged proposal retry preserves its revision; a new
  hold still invalidates prior evidence. Holding invalidates an earlier idle proposal; a stale
  revision cannot restore it.
- Proposal validation and time publication use the same process-shared atomic guard. There is no
  unprotected interval in which incoming work can retract a proposal after validation but before
  publication. Lock-free atomics are required by compile-time assertions.
- A process-local view joins two mappings without embedding virtual addresses in shared state.
  Children map the time region read-only and use a const participant view; only the authority view
  can call `Commit` or write time. The control region is
  writable by trusted native participants. This is not an isolation boundary against a compromised
  renderer; JavaScript access and Mojo registration authorization remain integration requirements.
- Participant identity includes a partition/execution scope and a non-reused registration ordinal.
  Slot reuse, a foreign scope and stale retirement/failure notifications cannot identify a new owner.
- The authority never spins on a held guard. `busy` requires retaining/retrying the operation, not
  releasing work. A confirmed native death poisons the execution without stealing the dead guard,
  dropping its vote or advancing time. A timeout alone is not confirmation.

The 64-slot bound is an explicit prototype capacity, not an advertised browser limit. Exhaustion
refuses admission; it never silently omits a participant. Authority operations must be serialized on
one authority sequence. Scope allocation in the probe is deterministic test bookkeeping; the engine
must supply an unguessable partition/execution identity.

## Checks

1. A child retracts an idle vote before work is enqueued; its old proposal is rejected and time stays
   held until the new revision proposes its next instant.
2. Two children execute 900 simulated callbacks at 37 and 53 microsecond intervals. Each callback
   and a simulated storage consumer read the same shared instant, including across host delay.
   Those are kernel consumers, not Chromium JavaScript or cookie services.
3. A child dies while holding the guard. The authority returns `busy` without blocking, confirms
   SIGKILL with `waitpid`, then fails all further progress. The clock remains unchanged.
4. New participants hold time, stale/foreign identities are refused, participant slot reuse is safe,
   backwards and unbounded commits are rejected, and full membership refuses another join.

## Required engine integration

The browser must allocate/register the two shared regions, bind the read-only time region to cookie
and quota services, and distribute it before application document creation. The renderer coordinator
must convert its actual main/worker state into proposals and retract them before every relevant task
post. It must handle `busy` without losing or prematurely enqueueing work. The browser must wake
schedulers after publication even when a renderer's main thread is suspended in a synchronous load.
All time-changing paths, including unblock and synchronous response arrival, must use this authority.

This standalone probe does not prove lock-order compatibility, scheduler wake delivery, renderer
isolation, recorder recovery or full browser frozen-boundary semantics.
