# Conformance

`perry-conformance` tests Perry-WIT differentially against Node.js.
A contract names one capability, such as `ecma.string.operations`, and supplies directed cases plus a property-based generator (`proptest`) of TypeScript programs and inputs.
Executable cases and their metamorphic variants run as compiled components in a Wasmtime host with WASI 0.3, fuel metering and memory limits, and under Node.js, except aggregate cases with exact integers beyond 2^53, which use an expected-value model.
The runner checks that each variant matches its oracle and that transformations preserve return values, evaluation traces and standard streams. Rejection cases and their variants must report the required diagnostic.

## Expected Outcomes

Each case declares what must happen:

| Outcome | Meaning |
| :--- | :--- |
| `EquivalentExecution` | The component produces the same observable result as Node.js. |
| `CompilerRejection` | The compiler rejects the program with a diagnostic, for constructs outside the subset. |
| `InjectedFaultDetection` | The harness detects a deliberately broken export adapter: a wrong value, a dropped effect or a leaked host resource. These cases test the harness itself. |
| `BoundedGrowth` | Fuel for an input of size 4n is at most 6 times the fuel for size n. This admits O(n log n) work and rejects quadratic growth (16 times). |

## Expected-Pass List

`passing.txt` in `perry-conformance` lists the contracts that must pass.
A listed contract that fails is a regression, and an unlisted contract that passes fails the run until it is added, so the list only grows.
`deferred.txt` records contracts outside the supported domain with their exclusion rationale.

```sh
cargo run -p perry-conformance -- check
cargo run -p perry-conformance -- check --select node.fs
cargo run -p perry-conformance -- list
cargo run -p perry-conformance -- doc
```

`--select` filters by contract ID prefix. `doc` prints the catalog table below.

## Dependency updates

The `Dependency updates` workflow runs daily at 04:17 UTC and supports manual runs.
Set the repository variable `DEPENDENCY_UPDATE_CADENCE` to `weekly` to run scheduled
updates on Mondays. Manual runs accept a seed, case count, depth and shrink limit.
The defaults are the original run's UTC date as `YYYYMMDD`, 8 cases per contract,
depth 3 and 100 shrink attempts. Rerunning a workflow retains its original date.

Configure the repository secret `GH_TOKEN_FOR_UPDATES` with a fine-grained GitHub
token restricted to this repository, granting Contents and Pull requests read/write.
It publishes one PR from `codex/update-dependencies`; ordinary PR CI runs on the
published head. No automatic merge is enabled.

Each refresh preserves existing branch commits, merges `main`, then commits Nix
input changes separately from the Perry/pliron revision pins and both Cargo
lockfiles. The baseline is the update branch immediately before that refresh,
including any human fixes. Merge conflicts and concurrent pushes stop publication
without replacing human commits. Dependency resolution failures retain the attempted
updates in a draft PR with the failed stage identified.

Both snapshots build once, then run four independent conformance shards in parallel.
The standard test suite also runs against the candidate. For example:

```sh
nix develop -c cargo run --locked -p perry-conformance -- check \
  --seed 20261031 --cases 8 --depth 3 --shrink-limit 100 \
  --shard 0/4 --output-dir target/conformance-shard-0
```

Shard indices start at zero. SHA-256 of the contract ID determines its shard;
sharding does not change a contract's generated sequence. Keep output directories
outside the source tree or under an ignored directory, since campaigns verify
that their sources remain unchanged.

Reports are checkpointed after each contract. Generated failures retain the original
case before shrinking. Both original and minimized concrete cases are replayed on
both snapshots, since dependency updates can change a generator's interpretation
of a seed. Missing reports, crashed processes and timeouts leave validation incomplete.
Newly passing contracts still require review of `passing.txt`.

The PR retains deduplicated cases under `dependency-evidence/`, with concrete
TypeScript variants, revision metadata and replay commands. These are review
evidence and are not automatically registered as permanent tests. Minimized cases
are the best found within the configured shrink budget. Detailed logs and generated
binaries remain in workflow artifacts for 14 days; shared runner binaries are kept
for 3 days.

Validation uses bounded campaigns and a 45-minute ceiling for build and standard
test jobs. Build and command durations are recorded separately. Queuing, cold builds
and subsequent PR CI can extend total elapsed time beyond an hour. A manual run
with `dry_run` enabled validates the selected workflow revision without publishing
to the persistent branch or PR.

## Catalog

{{#include conformance-catalog.md}}
