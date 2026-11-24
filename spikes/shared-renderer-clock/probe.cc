#include "clock.h"
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <new>
#include <optional>
#include <type_traits>
#include <signal.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

using namespace frontcap;
template<class T> concept CanPublishTime = requires(T& clock) { Commit(clock, 100); };
static_assert(CanPublishTime<SharedClock>);
static_assert(!CanPublishTime<ParticipantClock>);
static_assert(std::is_const_v<std::remove_reference_t<decltype(std::declval<ParticipantClock>().now)>>);
#define REQUIRE(condition) do { if (!(condition)) { \
  std::fprintf(stderr, "failure at %s:%d: %s\n", __FILE__, __LINE__, #condition); \
  std::abort(); } } while (false)

struct Mapping {
  SharedControl* control;
  std::atomic<int64_t>* time;
  std::optional<SharedClock> view;
  SharedClock* clock;
  Mapping() {
    auto* region = mmap(nullptr, sizeof(SharedControl), PROT_READ | PROT_WRITE,
                        MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    auto* time_region = mmap(nullptr, sizeof(std::atomic<int64_t>), PROT_READ | PROT_WRITE,
                             MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    REQUIRE(region != MAP_FAILED && time_region != MAP_FAILED);
    static uint64_t sequence = 0;
    // Probe-only identity; Chromium must supply an unguessable execution token.
    control = new(region) SharedControl({static_cast<uint64_t>(getpid()), ++sequence});
    time = new(time_region) std::atomic<int64_t>(0);
    view.emplace(*control, *time);
    clock = &*view;
  }
  ParticipantClock participant() {
    REQUIRE(mprotect(time, sizeof(std::atomic<int64_t>), PROT_READ) == 0);
    return {*control, *time};
  }
  ~Mapping() {
    munmap(control, sizeof(SharedControl));
    munmap(time, sizeof(std::atomic<int64_t>));
  }
};
void signal(int fd) { const char byte = 'x'; REQUIRE(write(fd, &byte, 1) == 1); }
void receive(int fd) { char byte; REQUIRE(read(fd, &byte, 1) == 1); }
void joined(pid_t child) {
  int status;
  REQUIRE(waitpid(child, &status, 0) == child);
  REQUIRE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
auto deadline() { return std::chrono::steady_clock::now() + std::chrono::seconds(5); }

void stale_vote_after_posting() {
  Mapping shared;
  auto& clock = *shared.clock;
  Handle first{}, second{};
  REQUIRE(Join(clock, first) == Status::ok);
  REQUIRE(Join(clock, second) == Status::ok);
  const auto initial = Propose(clock, first, 0, 100);
  REQUIRE(initial.status == Status::ok);
  const auto repeated = Propose(clock, first, initial.revision, 100);
  REQUIRE(repeated.status == Status::ok && repeated.revision == initial.revision);
  REQUIRE(Propose(clock, second, 0, 300).status == Status::ok);
  int posted[2], resume[2];
  REQUIRE(pipe(posted) == 0 && pipe(resume) == 0);
  const auto child = fork();
  REQUIRE(child >= 0);
  if (child == 0) {
    alarm(10);
    auto clock = shared.participant();
    const auto held = Hold(clock, first);
    REQUIRE(held.status == Status::ok && held.now == 0);
    signal(posted[1]);
    receive(resume[0]);
    REQUIRE(Propose(clock, first, held.revision, 200).status == Status::ok);
    _exit(0);
  }
  receive(posted[0]);
  REQUIRE(Propose(clock, first, 1, 500).status == Status::stale);
  REQUIRE(Commit(clock, 1000).status == Status::held);
  REQUIRE(clock.now.load() == 0);
  signal(resume[1]);
  joined(child);
  REQUIRE(Commit(clock, 250).now == 200);
  REQUIRE(Commit(clock, 199).status == Status::invalid);
  for (int fd : {posted[0], posted[1], resume[0], resume[1]}) close(fd);
}

void callbacks_share_time() {
  Mapping shared;
  auto& clock = *shared.clock;
  constexpr int64_t end = 37 * 53 * 10;
  constexpr int64_t expiry = 1001;
  Handle handles[2]{};
  pid_t children[2]{};
  for (auto& handle : handles) REQUIRE(Join(clock, handle) == Status::ok);
  for (size_t i = 0; i < 2; ++i) {
    children[i] = fork();
    REQUIRE(children[i] >= 0);
    if (children[i] == 0) {
      alarm(10);
      auto clock = shared.participant();
      const int64_t interval = i == 0 ? 37 : 53;
      uint64_t revision = 0;
      const auto until = deadline();
      for (int64_t at = interval; at <= end; at += interval) {
        Receipt proposal;
        do { proposal = Propose(clock, handles[i], revision, at); }
        while (proposal.status == Status::busy && std::chrono::steady_clock::now() < until);
        REQUIRE(proposal.status == Status::ok);
        revision = proposal.revision;
        while (clock.now.load(std::memory_order_acquire) < at) {
          REQUIRE(std::chrono::steady_clock::now() < until);
          usleep(50);
        }
        Receipt held;
        do { held = Hold(clock, handles[i]); }
        while (held.status == Status::busy && std::chrono::steady_clock::now() < until);
        REQUIRE(held.status == Status::ok && held.now == at);
        revision = held.revision;
        // This simulates application and storage consumers reading one region.
        // It is a commit-kernel oracle, not a Chromium cookie-service test.
        const auto callback_time = clock.now.load(std::memory_order_acquire);
        usleep(50);
        const auto storage_time = clock.now.load(std::memory_order_acquire);
        REQUIRE(callback_time == at && storage_time == at);
        REQUIRE((storage_time < expiry) == (at < expiry));
      }
      Receipt idle;
      do { idle = Propose(clock, handles[i], revision, kIdle); }
      while (idle.status == Status::busy && std::chrono::steady_clock::now() < until);
      REQUIRE(idle.status == Status::ok);
      _exit(0);
    }
  }
  const auto until = deadline();
  while (clock.now.load() < end) {
    const auto committed = Commit(clock, end);
    REQUIRE(committed.status == Status::ok || committed.status == Status::busy || committed.status == Status::held);
    REQUIRE(clock.now.load() <= end);
    REQUIRE(std::chrono::steady_clock::now() < until);
    usleep(50);
  }
  for (const auto child : children) joined(child);
  REQUIRE(Commit(clock, end).status == Status::held);
  REQUIRE(clock.now.load() == end);
}

void dead_guard_never_hangs_authority() {
  Mapping shared;
  auto& clock = *shared.clock;
  Handle handle{};
  REQUIRE(Join(clock, handle) == Status::ok);
  int ready[2];
  REQUIRE(pipe(ready) == 0);
  const auto child = fork();
  REQUIRE(child >= 0);
  if (child == 0) {
    alarm(10);
    auto clock = shared.participant();
    Guard guard(clock, handle.identity);
    REQUIRE(guard.status() == Status::ok);
    signal(ready[1]);
    for (;;) pause();
  }
  receive(ready[0]);
  // No retry/spin is hidden inside the authority transaction.
  REQUIRE(Commit(clock, 100).status == Status::busy);
  REQUIRE(kill(child, SIGKILL) == 0);
  int status;
  REQUIRE(waitpid(child, &status, 0) == child);
  REQUIRE(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
  REQUIRE(FailConfirmed(clock, handle) == Status::failed);
  REQUIRE(Commit(clock, 100).status == Status::failed);
  Handle another{};
  REQUIRE(Join(clock, another) == Status::failed);
  REQUIRE(clock.now.load() == 0);
  close(ready[0]); close(ready[1]);
}

void admission_and_identity() {
  Mapping shared;
  auto& clock = *shared.clock;
  REQUIRE(Commit(clock, 100).status == Status::empty);
  Mapping foreign;
  Handle foreign_handle{};
  REQUIRE(Join(*foreign.clock, foreign_handle) == Status::ok);
  Handle first{};
  REQUIRE(Join(clock, first) == Status::ok);
  REQUIRE(first.slot == foreign_handle.slot && first.identity == foreign_handle.identity);
  REQUIRE(Hold(clock, foreign_handle).status == Status::stale);
  REQUIRE(FailConfirmed(clock, foreign_handle) == Status::stale);
  REQUIRE(Commit(clock, 100).status == Status::held);
  REQUIRE(Propose(clock, first, 0, kIdle).status == Status::ok);
  REQUIRE(Commit(clock, 37).now == 37);
  Handle second{};
  REQUIRE(Join(clock, second) == Status::ok);
  REQUIRE(Commit(clock, 100).status == Status::held);
  REQUIRE(RetireConfirmed(clock, second) == Status::ok);
  Handle replacement{};
  REQUIRE(Join(clock, replacement) == Status::ok);
  REQUIRE(replacement.slot == second.slot && replacement.identity != second.identity);
  REQUIRE(Propose(clock, second, 0, 100).status == Status::stale);
  REQUIRE(FailConfirmed(clock, second) == Status::stale);
  REQUIRE(Propose(clock, replacement, 0, 36).status == Status::invalid);
  REQUIRE(Propose(clock, replacement, 0, 53).status == Status::ok);
  REQUIRE(Commit(clock, 100).now == 53);
  REQUIRE(Commit(clock, kIdle).status == Status::invalid);
  // Exhaustion refuses a new participant instead of silently omitting it.
  for (size_t i = 2; i < kCapacity; ++i) {
    Handle extra{};
    REQUIRE(Join(clock, extra) == Status::ok);
  }
  Handle overflow{};
  REQUIRE(Join(clock, overflow) == Status::full);
  REQUIRE(Commit(clock, 100).status == Status::held);
}

int main() {
  alarm(15);  // A failed interprocess handshake must not leave an unattended job.
  stale_vote_after_posting();
  callbacks_share_time();
  dead_guard_never_hangs_authority();
  admission_and_identity();
  std::puts("SHARED_CLOCK_KERNEL_PROBE {\"cases\":4,\"passed\":4,\"browser_qualification\":false}");
}
