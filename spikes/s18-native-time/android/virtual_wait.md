# Owned virtual-wait experiment

`virtual_wait.h` is a prototype for explicitly enrolled waits. The standalone controls do not
change ART, Java timer or syscall behavior. Normal runtime builds do not link the scoped interceptor;
the explicit `profile-saver` candidate links it through `profile_saver_wait.c`. It uses existing
futex operations because the recorded installed-app seccomp policy rejects `futex_waitv`.

Each stack-owned waiter has its own atomic futex word. Registration and completion share one
mutex. Notification or virtual deadline expiry sets the word before waking it, so an intervening
notification cannot be lost between registration and the kernel wait. Waiting uses an untimed
`FUTEX_WAIT_PRIVATE`; virtual advance selects expired waiters. The test's real-time sleeps and
five-second alarm are independent failure bounds, not part of the waiting algorithm.

The explicit requeue operation handles owned waiters only. It checks the expected source value,
wakes a bounded number, and changes the key for a bounded number of remaining waiters while
preserving their deadlines. All notifications/requeues for these keys must use this registry.
Direct kernel wakes on the original address cannot reach its synthetic waiter words.

Compile the standalone control with the Android NDK compiler, for example:

```sh
"$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android30-clang" \
  -Wall -Wextra -Werror -O1 virtual_wait_control.c -o virtual_wait_control
```

Run it only as an isolated owned test process on the guest. It checks frozen/pre-deadline state,
exact expiry, notification and advance before kernel registration, source-value rejection,
requeue/destination notification, backward-advance rejection, bounded multi-waiter wake/requeue
counts, targeted non-restarting signal interruption, and twenty notification/deadline races. The retained JNI experiment
runs the same control in an installed fixture, proving those operations are permitted there;
its temporary APK modification is not part of the production fixture.

The tested signal interruption removes its waiter; this does not establish pthread cancellation
or arbitrary process teardown. The race comparisons accept either serialized winner and verify
consistent wake counts; twenty runs do not prove every interleaving. Before integration, verify
broader concurrent lifetime handling. A generic syscall interception layer would
add ABI forwarding, errno behavior, mixed enrolled/kernel waiters, and other futex operations. Those are not implemented here. No Profile Saver repair, arbitrary futex support, or historical
retirement-timeout closure follows from this control passing.

## ARM64 forwarding experiment

`syscall_forward.S` is a separate transparent syscall entry derived from the Android 15 Bionic
ARM64 convention; it retains the source license. It moves the syscall number and six argument
registers into the kernel ABI, keeps successful calls frameless, and translates kernel errors
into thread-local errno. It is not linked by the normal build. The isolated APK experiment links
it into its preload library without connecting the virtual-wait registry.

`syscall_forward_control.c` checks getpid and preservation of errno on success, EBADF, pipe I/O,
anonymous mmap/munmap and the sixth futex argument through invalid/valid bitset controls. Define
`SYSCALL_FORWARD_STANDALONE` to build an isolated executable with the assembly file. The APK
experiment invokes the same control through JNI and reruns the owned-wait controls.

A passing forwarding-control APK is not proof that every ART/internal libc call binds this entry. Clone, signal return,
restartable syscalls, alternate ABIs and generic cancellation are outside these controls; a
frameless entry is a design constraint, not a runtime test of all those operations.

## Target-call observation

## Scoped Profile Saver intervention

`profile_saver_wait.c` and `FRONTCAP_VIRTUAL_FUTEX` form an isolated integration experiment.
The normal build still links neither. A candidate build connects `fc_virtual_now` to the manual
clock offset and calls `fc_advance_saver_waits` after advancing it. The first timed private futex
wait from the fixture's `Profile Saver` names the condition key; subsequent timed/untimed waits
on that key use the owned registry. This is a thread-name/operation enrollment rule, not a
portable ART API or proof of identity for arbitrary applications.

Condition notifications, unconditional requeue and compare-requeue use the registry. A requeue destination may also
have ordinary kernel waiters: wake counts first cover selected owned waiters and then pass the
remaining count to the kernel. The candidate rejects unsupported condition operations, differently
tagged aliases, additional condition-waiting threads and changing requeue destinations by exiting
92. It does not silently expand support. This boundary still assumes that condition operations
reach the interposed syscall entry; direct/unobserved syscalls are not covered.

The control includes unconditional requeue after the source sequence changes, matching ART's
RequeueWaiters path. Unsupported integration paths emit the frozen source line, TID and syscall
arguments to stderr before exit 92. A previously retained exit-92 failure lacks those details;
its precise branch cannot be reconstructed from the exit code alone.
