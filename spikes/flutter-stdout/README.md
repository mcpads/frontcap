# Existing Flutter runtime stdout observation

This bounded diagnostic subscribes to the VM service `Stdout` stream already exposed by some
Flutter debug hosts. It adds no consumer library, source hook, evaluation or result callback. Runtime support is optional; absence is unsupported, not permission to require a consumer test API. It is not integrated with native Device ownership. Arbitrary long output and other hosts remain unqualified.

The reference Android host already logs its data callback, but prior logcat evidence truncated a
long JSON line. This probe investigates whether existing VM stdout preserves those bytes before
platform-log truncation. Receiving a complete game console result is not proof of native host
receipt; a host stdout record must be identified separately. Successful subscription alone proves
neither that stdout is emitted nor that a game result is complete.

Under the existing native lock, acquire an owned target and verify its consumer PID/start tick.
Discover its VM-service endpoint from that process's own logs and create one owned loopback ADB
forward. Preserve the authentication path and freeze endpoint/owner provenance privately. Do not
connect to a shared or unrelated VM, remove another owner's forward or publish service tokens.

```sh
node spikes/flutter-stdout/stream.mjs ws://127.0.0.1:OWNED_PORT/AUTH_PATH/ws /fresh/output 120
```

Wait for `ready.json` before the selected journey. Create `/fresh/output/stop` to finish while the
VM remains alive, then collect the result and retire the exact forward and target. The caller owns
that supervision; the script does not discover devices, forward ports, kill apps or restart a VM.
A missing acknowledgement fails after 10 seconds; the entire observation is bounded to at most
900 seconds. Disconnection, malformed messages, rejected subscription and capacity exhaustion are
explicit failures with retained partial evidence, never automatic retries.

The limit is 256 events and 1 MiB of decoded stdout. Raw WriteEvents are appended to `events.jsonl`
in receive order; final `events.json` and `result.json` preserve the collection and disposition.
Stdout chunks are decoded separately per isolate after reassembly, so split UTF-8 characters and
interleaved isolates do not corrupt JSON records. Missing isolate identity is retained as null.
Incomplete UTF-8 at the observation boundary is a reported failure. This does not infer line or
result completion, remove prefixes, merge isolates or equate a time window with a full session.

Run `node --test spikes/flutter-stdout/stream.test.mjs` for event-before-ack, split Unicode,
isolate separation, capacity and malformed/rejected-message behavior. Transport and installed
host-result qualification remain separate. The VM-service message contract was checked against
the installed upstream `vm_service` package's `streamListen`, Stdout/WriteEvent and Event.bytes
API documentation; not every embedder provides this stream.
