## Which CI build-time fix, if any, should I make?

A one-line CSS change costs ~25 minutes: `test` ~10 min, then `publish`
~12-17 min, run in series because `publish` needs `[test, ui]`. The `ui` job,
the only one that could have failed on that diff, took 1 minute.

Causes, measured: no cache for `~/.cargo` or `target/` (the only `actions/cache`
in the workflow is for the classifier model, and ARC runners are ephemeral
pods), so Rust compiles cold three times per run — clippy, clippy with
features, then test. The Dockerfile then compiles it a fourth time, in release
mode for arm64, with no dependency layer to survive a source change.

Options:

- **Cache cargo and `target/` in the `test` job.** Biggest win for the least
  risk; warm runs drop to a couple of minutes. Helps every run, Rust or not,
  and restructures nothing.
- **Split the Dockerfile's dependency layer** (cargo-chef or a manifest-only
  pre-build) so a source change recompiles this crate rather than the whole
  tree. Touches the image build every deploy depends on.
- **Skip the Rust job when no Rust changed** (`paths-filter`). A web-only
  commit becomes ~1 min plus the image build. Needs a `needs:` restructure,
  not just a skip: the UI is embedded via `rust-embed`, so the image must
  still be rebuilt.
- **Leave it alone.** 25 minutes is tolerable and the pipeline is understood.

**Decided:** cache cargo and `target/` in the `test` job. The other two remain open.
