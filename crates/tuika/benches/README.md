# tuika benchmarks

Two suites with different jobs. Criterion measures wall-clock and is advisory;
iai-callgrind counts instructions and is a CI gate.

## Criterion (wall clock)

`markdown.rs` and `scroll.rs` are Criterion targets. Wall-clock numbers are too
noisy on shared runners to gate on, so CI runs these only on `main` (and via
manual dispatch) and uploads the Criterion output as an artifact. Regression
checking is local and baseline-to-baseline:

```bash
cargo bench -p tuika --bench markdown -- --save-baseline before
# ...make the change...
cargo bench -p tuika --bench markdown -- --baseline before
```

A new component bench goes in the owning crate's `benches/` as another
`[[bench]]` with `harness = false`.

## iai-callgrind (instruction counts)

The `*_iai` targets count CPU instructions under Valgrind/callgrind
([iai-callgrind](https://github.com/iai-callgrind/iai-callgrind)). Counts are
deterministic and machine-independent for a fixed toolchain + libc, so they are
committed to `iai-baseline.json` and the CI `iai` job **fails** on a regression
past the baseline's tolerance. This is a real gate, not an archive.

Running locally needs Valgrind and a version-matched runner
(`cargo install iai-callgrind-runner`):

```bash
rm -rf target/iai
cargo bench -p tuika --bench markdown_iai \
  -p tuika-codeformatters --bench highlight_iai -- --save-summary=json
python3 crates/tuika/benches/check_iai.py            # compare to the committed baseline
python3 crates/tuika/benches/check_iai.py --update   # bless new counts
```

Treat the baseline like a snapshot test: when a change legitimately shifts
counts (renderer change, dependency bump, toolchain upgrade), regenerate with
`--update` and commit it alongside the code. To refresh from CI's exact
environment, run the workflow manually (`workflow_dispatch`) and commit the
uploaded `iai-baseline` artifact.
