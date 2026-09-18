# Rust network transport policy

`reqwest::blocking` is deprecated for zed-cli network work.

New registry/resolver traffic must use the cancellable async transport from `zed-client-async`, which is built on Hyper, hyper-util, Hyper-Rustls, and Tokio. The CLI may retain synchronous command entry points, but network operations that can race local work must be futures with explicit cancellation rather than blocking socket calls hidden inside worker threads.

## Current migration boundary

The local-development resolver uses the async client for speculative package/version metadata and a `tokio_util::CancellationToken` to cancel that request when the complete dependency graph is already available from local checkouts.

The existing blocking registry implementation remains temporarily grandfathered for compatibility paths that have not yet moved:

1. artifact and binary-artifact body acquisition
2. source-host fallback after an async primary-registry error
3. multipart publication and upload paths

No new `reqwest::blocking` call sites may be added. Those remaining paths should move to streaming Hyper bodies and then the `reqwest` dependency can be removed entirely.
