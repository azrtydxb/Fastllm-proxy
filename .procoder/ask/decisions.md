## Verify the red laggard banner against a genuinely frozen replica on kw?

The quiet path is verified live: a real 25s transient produced the neutral
"still picking up the snapshot" note and no red banner, in the browser, on the
deployed build.

The red path — a replica that is actually stuck — is covered only by tests:
the render test drives the real component against a 29-day-old fixture
snapshot and asserts both red markers, and a unit test simulates polling a
frozen replica while the snapshot keeps republishing underneath it. That is
automated, but it is not the cluster.

Options:

- **Leave it at test coverage.** Nothing further to do; the alert path is
  exercised by the real component, just not against live pods.
- **Freeze a replica on kw and watch the banner turn red.** Scale the proxy
  deployment and start one replica with `FASTLLM_CONFIG_POLL=0`, so it holds
  its snapshot while the others advance. Touches the live deployment and
  leaves one replica serving a stale config until it is removed; the gateway
  keeps serving throughout, and the extra replica is deleted afterwards.

**Decided:** leave it at test coverage. No live deployment change made.
