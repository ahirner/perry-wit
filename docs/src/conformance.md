# Conformance

Perry-WIT verifies source equivalence and WASI 0.3 host integration through `perry-conformance`, an automated differential verification harness that tests compiled components against **Node.js** as an executable specification oracle.

## Verification Topology

Every capability contract defines a parameter generator and a deterministic seed. During verification, the harness synthesizes TypeScript programs for each input variant and executes them across two parallel pipelines:

<figure class="pw-diagram">
<svg viewBox="0 0 780 230" xmlns="http://www.w3.org/2000/svg" role="img" aria-label="Perry-WIT Differential Conformance Verification Topology">
  <defs>
    <marker id="v-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 1 L 8 5 L 0 9 z" fill="currentColor" opacity="0.65" />
    </marker>
    <marker id="v-arrow-blue" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 1 L 8 5 L 0 9 z" fill="#3b82f6" />
    </marker>
  </defs>
  <rect x="20" y="75" width="185" height="64" rx="6" fill="currentColor" fill-opacity="0.08" stroke="currentColor" stroke-opacity="0.4" />
  <text x="112" y="103" text-anchor="middle" fill="currentColor" font-size="12" font-weight="600">Capability Contract</text>
  <text x="112" y="122" text-anchor="middle" fill="currentColor" opacity="0.7" font-size="11">Seeded Input Variants</text>
  <path d="M 205 95 C 245 95, 245 48, 285 48" fill="none" stroke="currentColor" stroke-opacity="0.5" stroke-width="1.5" marker-end="url(#v-arrow)" />
  <path d="M 205 119 C 245 119, 245 166, 285 166" fill="none" stroke="#3b82f6" stroke-width="2" marker-end="url(#v-arrow-blue)" />
  <rect x="290" y="20" width="230" height="56" rx="6" fill="currentColor" fill-opacity="0.06" stroke="currentColor" stroke-opacity="0.35" />
  <text x="405" y="45" text-anchor="middle" fill="currentColor" font-size="12" font-weight="600">Node.js Reference Oracle</text>
  <text x="405" y="63" text-anchor="middle" fill="currentColor" opacity="0.7" font-size="11">Value, Error &amp; Effect Trace</text>
  <rect x="290" y="138" width="230" height="56" rx="6" fill="#3b82f6" fill-opacity="0.12" stroke="#3b82f6" stroke-width="1.5" />
  <text x="405" y="163" text-anchor="middle" fill="currentColor" font-size="12" font-weight="600">Perry-WIT + Wasmtime P3 Host</text>
  <text x="405" y="181" text-anchor="middle" fill="currentColor" opacity="0.7" font-size="11">Canonical ABI, Fuel &amp; Resource Audit</text>
  <path d="M 520 48 C 560 48, 560 95, 595 95" fill="none" stroke="currentColor" stroke-opacity="0.5" stroke-width="1.5" marker-end="url(#v-arrow)" />
  <path d="M 520 166 C 560 166, 560 119, 595 119" fill="none" stroke="#3b82f6" stroke-width="2" marker-end="url(#v-arrow-blue)" />
  <rect x="600" y="75" width="160" height="64" rx="6" fill="currentColor" fill-opacity="0.1" stroke="currentColor" stroke-opacity="0.5" stroke-width="1.5" />
  <text x="680" y="103" text-anchor="middle" fill="currentColor" font-size="12" font-weight="600">Ratchet Gate</text>
  <text x="680" y="122" text-anchor="middle" fill="currentColor" opacity="0.7" font-size="11">Equivalence + O(n) Fuel</text>
</svg>
<figcaption>Differential oracle comparison paired with Wasmtime P3 resource and fuel scaling checks.</figcaption>
</figure>

## Execution Ratchet

The conformance runner enforces a monotonic quality ratchet across four dimensions:

1. **Differential Equivalence**: Compiled WASI 0.3 components must produce the exact return value, stdout/stderr trace, or exception class produced by the Node.js oracle across all generated variants.
2. **Diagnostic Boundary Verification**: Contracts with expected outcome `CompileDiagnostic` or `RuntimeError` verify that unsupported dynamic constructs or invalid boundary inputs fail with deterministic diagnostics rather than silent miscompilation.
3. **Linear Fuel Scaling**: Scaling contracts execute proportional input sizes ($n$ and $4n$) under Wasmtime fuel metering and assert that fuel consumption grows linearly ($F(4n) / F(n) \le 6\times$), preventing accidental quadratic regressions in string, array, or stream handling.
4. **Monotonic Registration (`passing.txt`)**: Every passing contract is recorded in `crates/conformance/passing.txt`. If a passing contract regresses—or if a newly passing contract is not registered—`perry-conformance check` fails the build.

## Harness CLI

Run the conformance suite locally from the workspace root:

```sh
# Execute all registered capability contracts and ratchet checks
cargo run -p perry-conformance -- check

# Filter to a specific capability prefix (e.g. node.fs, web.fetch, wasi.p3)
cargo run -p perry-conformance -- check --select node.fs

# List all registered contract IDs
cargo run -p perry-conformance -- list

# Emit the Markdown capability catalog printed below
cargo run -p perry-conformance -- doc
```

## Capability Catalog

The tables below are generated directly from the authoritative contract registry (`crates/conformance/src/registry.rs` and `crates/conformance/passing.txt`) at documentation build time.

{{#include conformance-catalog.md}}
