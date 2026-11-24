# Native shared-hold regression check

`shared-hold-wakeup.patch` adds a test-only peer and a focused coordinator test.
It is an overlay, not an engine patch: it does not change the installed engine identity.
The test keeps the committed clock fixed with two local participants, invalidates one participant's
future vote with `Hold`, and checks that notifier processing requests a scheduler wake without
advancing time or releasing the hold. Unchanged polls must not repeat the wake; multiple holds
before a poll must coalesce. This isolates notification behavior; it does not reproduce the full
intermittent browser failure or exercise the notifier's real timer.

With depot_tools on PATH and the matching Chromium/V8 checkout already configured in
`out/frontcap`, run:

```sh
python3 engine/tests/verify_shared_hold.py \
  --src /path/to/chromium/src --repo "$PWD" \
  --evidence /path/to/new/evidence --jobs 12
```

The script takes the normal engine build lock and requires clean tracked Chromium/V8 files.
It applies the repository's engine/V8 patches and the test overlay, builds only
`blink_platform_unittests`, and first restores the old notifier condition for a counterfactual.
That exact test must fail on wake-count assertions. It then restores the candidate and runs all
`ProcessTimeOverrideCoordinatorTest` cases. It does not build/install another headless artifact.
The initial native test target build can be substantial; it is separate from the headless delivery
build. Logs, source hashes, XML results and cleanup status are retained in the evidence directory.

Normal exit and handled errors restore the changed checkout files. If the process is killed or
cleanup fails, inspect `native-status.json` and the retained source patches before resuming; do not
reset an unrelated checkout or overwrite the old evidence directory. Do not run another engine
build against the locked checkout. Concurrent runtime checks need recorded, disjoint physical CPU
allocations and sufficient memory/storage headroom; otherwise use a separate host or wait.
