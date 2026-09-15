# Local lock telemetry contract

Zed local lock ownership is enforced by `zed-lock` descriptor/handle locks. Telemetry is observation only and must never become ownership evidence.

## Allowed fields

A consumer-facing `ores-otel` lock event may emit only bounded, low-cardinality fields derived from the operation class rather than the concrete lock identity:

- `backend = "zed-lock"`
- `event = waiting | acquired | contended | released | timed_out | cancelled | failed`
- `lock_class = project_mutation | refs | artifact | build | install | other`
- `operation = <reviewed stable operation id>`
- `elapsed_ms = <non-negative integer>` when applicable
- `outcome = success | contention | timeout | cancelled | failure`

Metrics may aggregate acquisition latency, wait latency, contention counts, timeout/cancellation counts, and failures by the bounded fields above.

## Prohibited fields

Do not export any of these to telemetry, logs, metric labels, or tracing baggage:

- owner/PID diagnostic records as ownership evidence;
- raw lock file paths, `$HOME`, usernames, project paths, package-private paths, or temp paths;
- credentials, registry/auth tokens, URLs containing credentials, or environment values;
- raw package names/versions as metric labels when an operation/class dimension is sufficient;
- artifact SHA-256/build keys as labels (they are high cardinality);
- the contents of zed-lock rendezvous files.

If a trace needs a per-operation correlation value, generate a fresh opaque trace/span identifier in the telemetry layer rather than reusing the filesystem lock identity.

## Failure policy

Telemetry failure must not prevent lock acquisition or release. Conversely, telemetry must not report `acquired` until native ownership has actually been granted, and it must not infer ownership from a rendezvous file's existence or diagnostic contents.
