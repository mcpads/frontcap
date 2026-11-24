# Foreground broadcast queue workload

This diagnostic APK supplies four exported, DUMP-protected receivers in separate processes. It has
no Activity, native library or virtual-clock wrapper. Each explicit request must contain a fresh
UUID token and `hold_ms` equal to 0 (warmup) or 8000 (load). A receiver persists its PID, main-thread
ID, monotonic start and finish in a token-bound AtomicFile. The eight-second polling budget is
below the observed ten-second foreground receiver timeout; scheduling stalls can still affect it.

Build with the installed API 35 SDK/JDK:

```sh
bash spikes/s18-native-time/android/broadcast-load/build.sh EMPTY_OUTPUT
```
