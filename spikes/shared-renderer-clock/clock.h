// Shared-clock proposal/commit kernel. Transport preparation does not activate it.
#ifndef FRONTCAP_SHARED_CLOCK_H_
#define FRONTCAP_SHARED_CLOCK_H_
#include <array>
#include <atomic>
#include <cstdint>
#include <cstddef>
#include <limits>

namespace frontcap {
// No fallback atomic implementation may keep a lock in process-local memory.
static_assert(std::atomic<uint64_t>::is_always_lock_free);
static_assert(std::atomic<int64_t>::is_always_lock_free);
static_assert(std::atomic<bool>::is_always_lock_free);
constexpr uint64_t kAuthority = std::numeric_limits<uint64_t>::max();
constexpr int64_t kIdle = std::numeric_limits<int64_t>::max();
constexpr size_t kCapacity = 64;

enum class Status { ok, busy, failed, stale, invalid, full, held, empty };
struct Scope {
  uint64_t high = 0;
  uint64_t low = 0;
  bool operator==(const Scope&) const = default;
};
struct Handle { Scope scope; size_t slot; uint64_t identity; };
struct Receipt {
  Status status;
  int64_t now = 0;
  uint64_t revision = 0;
};
struct Participant {
  // Registration/retirement belong to one browser authority. Identity also permits
  // confirmed failure to poison the clock without acquiring a dead process's guard.
  std::atomic<uint64_t> identity{0};
  std::atomic<bool> armed{false};
  int64_t proposed = 0;
  uint64_t revision = 0;
};
struct SharedControl {
  // The browser supplies an unguessable identity for this partition/execution.
  // Local participant ordinals alone collide with another region or a restart.
  explicit SharedControl(Scope identity) : scope(identity) {}
  const Scope scope;
  std::atomic<uint64_t> guard{0};
  std::atomic<bool> failed{false};
  std::atomic<bool> activated{false};
  std::array<Participant, kCapacity> participants;
  uint64_t next_identity = 1;
};

// A process-local view: shared regions contain no pointers. The time region is
// writable only in the authority; participants map it read-only. Both mappings
// may have different virtual addresses in each process.
struct ParticipantClock {
  ParticipantClock(SharedControl& state, const std::atomic<int64_t>& time)
      : control(state), now(time) {}
  SharedControl& control;
  const std::atomic<int64_t>& now;
};
struct SharedClock : ParticipantClock {
  SharedClock(SharedControl& state, std::atomic<int64_t>& time)
      : ParticipantClock(state, time), writable_now(time) {}
  std::atomic<int64_t>& writable_now;
};

// Never block the browser sequence on a process that might have crashed or stopped.
// Callers retain work on busy. In particular, a poster must acquire a hold BEFORE
// timestamping/enqueuing application work; busy is not permission to enqueue it.
class Guard {
 public:
  Guard(ParticipantClock& clock, uint64_t owner) : clock_(clock) {
    if (!owner) { status_ = Status::invalid; return; }
    if (clock.control.failed.load(std::memory_order_acquire)) { status_ = Status::failed; return; }
    uint64_t empty = 0;
    if (!clock.control.guard.compare_exchange_strong(empty, owner, std::memory_order_acquire)) return;
    owned_ = true;
    status_ = clock.control.failed.load(std::memory_order_acquire) ? Status::failed : Status::ok;
  }
  ~Guard() { if (owned_) clock_.control.guard.store(0, std::memory_order_release); }
  Guard(const Guard&) = delete;
  Guard& operator=(const Guard&) = delete;
  Status status() const { return status_; }
 private:
  ParticipantClock& clock_;
  bool owned_ = false;
  Status status_ = Status::busy;
};

inline bool Matches(const ParticipantClock& clock, Handle handle) {
  return handle.scope == clock.control.scope && handle.slot < kCapacity && handle.identity != 0 && handle.identity != kAuthority &&
      clock.control.participants[handle.slot].identity.load(std::memory_order_acquire) == handle.identity;
}

// Authority-only methods must be serialized on the browser authority sequence.
inline Status Join(SharedClock& clock, Handle& handle) {
  Guard guard(clock, kAuthority);
  if (guard.status() != Status::ok) return guard.status();
  if (clock.control.scope == Scope{}) return Status::invalid;
  if (clock.control.next_identity == kAuthority) return Status::full;
  for (size_t slot = 0; slot < kCapacity; ++slot) {
    auto& participant = clock.control.participants[slot];
    if (participant.identity.load(std::memory_order_relaxed)) continue;
    participant.armed.store(false, std::memory_order_relaxed);
    participant.proposed = clock.now.load(std::memory_order_relaxed);
    participant.revision = 0;
    handle = {clock.control.scope, slot, clock.control.next_identity++};
    participant.identity.store(handle.identity, std::memory_order_release);
    return Status::ok;
  }
  return Status::full;
}

inline Receipt Hold(ParticipantClock& clock, Handle handle) {
  if (!Matches(clock, handle)) return {Status::stale};
  Guard guard(clock, handle.identity);
  if (guard.status() != Status::ok) return {guard.status()};
  if (!Matches(clock, handle)) return {Status::stale};
  auto& participant = clock.control.participants[handle.slot];
  if (participant.revision == kAuthority) return {Status::invalid};
  participant.proposed = clock.now.load(std::memory_order_relaxed);
  return {Status::ok, participant.proposed, ++participant.revision};
}

inline Receipt Propose(ParticipantClock& clock, Handle handle, uint64_t revision, int64_t earliest) {
  if (!Matches(clock, handle)) return {Status::stale};
  Guard guard(clock, handle.identity);
  if (guard.status() != Status::ok) return {guard.status()};
  if (!Matches(clock, handle)) return {Status::stale};
  auto& participant = clock.control.participants[handle.slot];
  const auto now = clock.now.load(std::memory_order_relaxed);
  if (revision != participant.revision) return {Status::stale, now, participant.revision};
  if (earliest < now || revision == kAuthority) return {Status::invalid, now, revision};
  // Retrying an unchanged proposal is not new producer work. Holds still
  // invalidate every earlier proposal before work is posted or admitted.
  if (participant.proposed == earliest) return {Status::ok, now, revision};
  participant.proposed = earliest;
  return {Status::ok, now, ++participant.revision};
}

inline Receipt Commit(SharedClock& clock, int64_t authorized_bound) {
  Guard guard(clock, kAuthority);
  if (guard.status() != Status::ok) return {guard.status()};
  const auto now = clock.now.load(std::memory_order_relaxed);
  if (authorized_bound < now || authorized_bound == kIdle) return {Status::invalid, now};
  auto next = authorized_bound;
  bool any = false;
  for (const auto& participant : clock.control.participants) {
    if (!participant.identity.load(std::memory_order_relaxed)) continue;
    any = true;
    if (participant.proposed < next) next = participant.proposed;
  }
  if (!any) return {Status::empty, now};
  if (next <= now) return {Status::held, now};
  // Validation and publication are one transaction. An incoming task cannot retract
  // its vote between this decision and publication of the common instant.
  clock.writable_now.store(next, std::memory_order_release);
  return {Status::ok, next};
}

inline Status RetireConfirmed(SharedClock& clock, Handle handle) {
  Guard guard(clock, kAuthority);
  if (guard.status() != Status::ok) return guard.status();
  if (!Matches(clock, handle)) return Status::stale;
  clock.control.participants[handle.slot].identity.store(0, std::memory_order_release);
  return Status::ok;
}

// A watchdog timeout is not confirmation. The browser must establish native death
// and fail the execution. Poisoning never steals a guard or drops a dead vote to
// continue. A new execution requires a new region.
inline Status FailConfirmed(SharedClock& clock, Handle handle) {
  if (!Matches(clock, handle)) return Status::stale;
  clock.control.failed.store(true, std::memory_order_release);
  return Status::failed;
}
}  // namespace frontcap
#endif
