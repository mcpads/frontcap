# Workspace Directory and Identifiers

## Local state directory

The workspace directory (`FRONTCAP_WORKSPACE`, by default `.frontcap/` in the MCP server's working
directory) holds local state and evidence. It is not committed.

```
.frontcap/
  <runtime>-<platform>.json  native host profiles (`native`, `host`, `webview` or `flutter`, for
                     `android` or `ios`) that a native launch reads
  launches/<request_id>.json  launch registry entry, created exclusively before any process starts
  runtimes/<launch_id>/
    spec.json        launch arguments, written by the launcher before the recorder starts
    recorder.lock    held exclusively by the runtime's one recorder for its lifetime; a second recorder
                     started for the same runtime exits without touching it
    runtime.json     `launch_id`, `state` (`starting`, `ready`, `stopped`, or `failed` with a reason),
                     process identity of the `recorder` and the `browser`, CDP `endpoint`, recorder
                     `socket`; written by the recorder, and by offline finalization when it records
                     a ready execution whose recorder and browser ended as failed
    finalization.json  last offline finalization outcome, separate from the execution failure
    control.json     persisted control state
    observations.json  the control state's node references and recent snapshots, saved when an
                     observation changes them and read again by a relaunched recorder
    journal.jsonl    request journal (intent, dispatched, completed)
    profile/         the browser's profile directory; the browser outlives a faulted recorder
    recorder.log     the recorder's standard error
    proxy-keys.json  the proxy's certificate authority keys, so a relaunched recorder's proxy presents
                     what the browser already trusts
    recorder.sock    the recorder's socket, unless the directory is too deep for one (see below)
    construction.json  a branch child's construction outcome (ready phases or the failed phase), so a
                     retried branch reports it again
    cell/            on a Linux host with cells (`FRONTCAP_CELLS`), the browser's cell directory, mounted
                     inside the cell; the runtime directory is then the recorder user's alone
  runs/<run_id>/     evidence bundle
    stopped-render.json  adapter's atomic completed-stop render batch, native opportunity and
                     clock/document identities; cleared to null when the retained batch completes
  results/<launch_id>/<sha256>.json  retained oversized MCP readbacks (not bundle members)
  saved/<saved_id>/  immutable saved basis: manifest.json (canonical), sha256, files/<sha256>.<ext>,
                     bundle/<run_id>/ (a live generation's finalized prefix); written once through a
                     private directory and a rename, verified on every read
  images/<name>/     a process image of a generation's browser in a cell (Linux): `images/` (CRIU's
                     image files), `cell/`, `run/` (the run so far), `proxy-keys.json`, `control.json`,
                     `spec.json`, `endpoint.json`; the root and each image are the recorder user's alone.
                     A checkpoint basis's image is named by its saved ID (a staged one `s-<hex>` until
                     saved); its manifest is under `saved/`
  replay-cassettes-<pid>-<nanos>/  cassette copies a relocated replay reads, removed when it ends (a
                     crash can leave one behind)
```

A native launch's runtime directory holds `native-spec.json` in place of `spec.json`, plus
`native-status.json`, `native-requests/`, its session (`native-session/` or a registered external
location), an optional controlled-service `service/`, write-budget and storage-root records and
`recorder-diagnostics/`.

## Recorder socket

The recorder listens on a Unix domain socket, `recorder.sock` inside its runtime directory. A runtime
directory too deep for a socket path (over 100 bytes) gets its socket at `/tmp/frontcap-<16 hex>.sock`,
named from a digest of the directory; the path in use is recorded in `runtime.json`. Windows, which
would need a named pipe in place of the socket, is not supported.

## Identifiers

Managed identifiers such as `launch_id`, `run_id`, rule ids, request ids, watch names and saved IDs
consist of lowercase ASCII letters and digits joined by single hyphens, at most 128 bytes long. Dots,
underscores, and path separators are rejected. Use returned identifiers verbatim; never derive paths
or other names from them.

- A launch ID is `g-<16 hex>`; its run ID is the same with `r-`. A saved ID is `b-` and the first 16
  hex digits of the basis manifest's SHA-256. Node references are `e<n>-<6 hex>`, the hex naming the
  issuing generation.
- Request IDs the server makes are `mcp-<16 hex>`, and steps derived from a request are
  `mcp-<role>-<id>`. An agent's request IDs and watch names may not start with `mcp-`, and its request
  IDs are at most 100 characters.
