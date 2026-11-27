//! Static conformance catalog verified against executable contract definitions.
use std::fmt::Write;

/// Contracts the compiler must pass, one id per line. The list only grows: a listed contract
/// that fails is a regression, and an unlisted contract that passes fails the campaign until it
/// is added. Unlisted failures are `pending`.
pub(crate) const PASSING: &str = include_str!("../passing.txt");

pub(crate) fn passing(id: &str) -> bool {
    PASSING.lines().any(|line| line.trim() == id)
}

/// Contracts outside the accepted domain, each with the deviation it records: not run, and
/// neither passing nor pending.
const DEFERRED: &str = include_str!("../deferred.txt");

/// `(id, reason)` of each deferred contract.
pub(crate) fn deferrals() -> impl Iterator<Item = (&'static str, &'static str)> {
    DEFERRED
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match line.split_once('#') {
            Some((id, reason)) => (id.trim(), reason.trim()),
            None => (line, ""),
        })
}

/// The deviation a deferred contract records.
pub(crate) fn deferred(id: &str) -> Option<&'static str> {
    deferrals()
        .find(|(deferred, _)| *deferred == id)
        .map(|(_, reason)| reason)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(conformance_doc_bin), derive(serde::Serialize))]
#[cfg_attr(not(conformance_doc_bin), serde(rename_all = "snake_case"))]
pub(crate) enum ExpectedOutcome {
    EquivalentExecution,
    CompilerRejection,
    InjectedFaultDetection,
    BoundedGrowth,
}

impl ExpectedOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::EquivalentExecution => "Equivalent execution",
            Self::CompilerRejection => "Compiler rejection",
            Self::InjectedFaultDetection => "Fault detection",
            Self::BoundedGrowth => "Bounded fuel growth",
        }
    }
}

/// A contract's documented metadata, in registry order. `contracts()` checks the partitions and
/// expected outcomes against the executable witnesses, so the table cannot drift.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(conformance_doc_bin), derive(serde::Serialize))]
pub struct CatalogEntry {
    pub(crate) id: &'static str,
    pub(crate) description: &'static str,
    pub(crate) specification: &'static str,
    pub(crate) domain: &'static str,
    pub(crate) partitions: &'static [&'static str],
    pub(crate) expected_outcomes: &'static [ExpectedOutcome],
}

const ECMA262: &str = "https://tc39.es/ecma262/";
const NODE_API: &str = "https://nodejs.org/api/";
const NODE_FS: &str = "https://nodejs.org/api/fs.html";
const NODE_TIMERS: &str = "https://nodejs.org/api/timers.html";
const WHATWG_FETCH: &str = "https://fetch.spec.whatwg.org/";
const WHATWG_STREAMS: &str = "https://streams.spec.whatwg.org/";
const COMPONENT_MODEL: &str = "https://component-model.bytecodealliance.org/";
const PROMISE_SPEC: &str = "https://tc39.es/ecma262/#sec-promise-objects";
const SCALING_SPEC: &str = "Implementation cost bound, not an ECMAScript requirement";

const CORE_DOMAIN: &str = "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/loop-carried/inert-exception forms and overlapping timer completion schedules";
const TIMER_DOMAIN: &str = "Retained async operation interleaved with typed numeric evaluation; instance reused across boundary inputs";
const CLI_DOMAIN: &str = "Standard CLI source; exact output bytes and successful exit";
const FAULT_DESC: &str = "Injected faults must be distinguished from the unchanged program";
const FAULT_DOMAIN: &str = "Value corruption, omitted effects, and retained host resources; only the component adapter is faulted";
const FETCH_DOMAIN: &str = "GET loopback HTTP; 200 and 404; binary bodies, explicit body cap and producer truncation; three calls per instance";
const BYTES_DOMAIN: &str = "Uint8Array sources of 0..256 bytes; each source evaluated over every IEEE-754 boundary input; byte reads checked explicitly";
const COMBINATOR_DOMAIN: &str = "Two numeric tasks; one fulfillment and one rejection for allSettled, two already-completed contenders for race; repeated component calls and IEEE-754 inputs";
const SCALING_DOMAIN: &str = "Warm-up n, n, and 4n calls on one instance; n in 32..=128 generated, 128 directed; values equal Node, fuel ratio of the last two calls at most 6";

const STREAM_PARTITIONS: &[&str] = &[
    "empty",
    "byte-boundary",
    "transfer-boundary",
    "beyond-guest-memory",
    "cancel-and-reuse",
];
const FETCH_PARTITIONS: &[&str] = &[
    "empty",
    "binary-exact-limit",
    "non-success-status",
    "overflow-cancellation",
    "completion-error",
];
const BYTES_PARTITIONS: &[&str] = &["empty", "binary-subview"];

const fn catalog_entry(
    id: &'static str,
    description: &'static str,
    specification: &'static str,
    domain: &'static str,
    partitions: &'static [&'static str],
    expected_outcomes: &'static [ExpectedOutcome],
) -> CatalogEntry {
    CatalogEntry {
        id,
        description,
        specification,
        domain,
        partitions,
        expected_outcomes,
    }
}

const fn equiv(
    id: &'static str,
    description: &'static str,
    specification: &'static str,
    domain: &'static str,
    partitions: &'static [&'static str],
) -> CatalogEntry {
    catalog_entry(
        id,
        description,
        specification,
        domain,
        partitions,
        &[ExpectedOutcome::EquivalentExecution],
    )
}

const fn core(
    id: &'static str,
    description: &'static str,
    partitions: &'static [&'static str],
) -> CatalogEntry {
    equiv(id, description, ECMA262, CORE_DOMAIN, partitions)
}

const fn cli(id: &'static str, description: &'static str) -> CatalogEntry {
    equiv(id, description, NODE_API, CLI_DOMAIN, &["standard-source"])
}

const fn fault(id: &'static str) -> CatalogEntry {
    catalog_entry(
        id,
        FAULT_DESC,
        COMPONENT_MODEL,
        FAULT_DOMAIN,
        &["injected-fault"],
        &[ExpectedOutcome::InjectedFaultDetection],
    )
}

const fn scaling(id: &'static str, description: &'static str) -> CatalogEntry {
    catalog_entry(
        id,
        description,
        SCALING_SPEC,
        SCALING_DOMAIN,
        &["n-128"],
        &[ExpectedOutcome::BoundedGrowth],
    )
}

const fn reject(
    id: &'static str,
    description: &'static str,
    specification: &'static str,
    domain: &'static str,
    partitions: &'static [&'static str],
) -> CatalogEntry {
    catalog_entry(
        id,
        description,
        specification,
        domain,
        partitions,
        &[ExpectedOutcome::CompilerRejection],
    )
}

pub(crate) static ENTRIES: &[CatalogEntry] = &[
    core(
        "ecma.number.arithmetic",
        "IEEE arithmetic and evaluation order",
        &[
            "signed-zero",
            "division",
            "trace",
            "call",
            "record-field-update",
            "byte-alias",
            "checked-json",
            "handled-throw",
        ],
    ),
    equiv(
        "ecma.number.bitwise",
        "Number bitwise operators, shifts and Math.imul",
        ECMA262,
        "Number operands over the IEEE-754 boundary corpus; ToInt32/ToUint32 wrapping, non-finite inputs, masked shift counts, signed and unsigned results, evaluation order, compound assignments and loop-carried checksums; direct/local/branch/loop/loop-carried/inert-exception forms",
        &[
            "and",
            "or",
            "xor",
            "not",
            "left",
            "right",
            "unsigned-right",
            "imul",
            "trace",
            "counts-and-compound-assignments",
        ],
    ),
    core(
        "ecma.math.rounding",
        "Numeric Math operations",
        &["floor", "ceil", "trunc", "abs", "round", "abs-widened-loop"],
    ),
    core(
        "ecma.string.operations",
        "String search, casing, slicing and length",
        &[
            "unicode-and-nul",
            "search",
            "lower",
            "upper",
            "slice",
            "char-at",
        ],
    ),
    core(
        "ecma.array.dense",
        "Dense numeric arrays preserve values and effects",
        &[
            "index",
            "branch-true",
            "branch-false",
            "first-index",
            "array-effects",
        ],
    ),
    core(
        "ecma.object.records",
        "Record fields preserve values and effects",
        &["field-order"],
    ),
    core(
        "ecma.date.epoch",
        "Date TimeClip and epoch reads",
        &["timeclip", "nested-codecs"],
    ),
    core(
        "ecma.uint8array.conversion",
        "Byte conversion and indexed access",
        &["wrapping", "byte-input", "byte-effects"],
    ),
    core(
        "web.textencoder.subviews",
        "UTF-8 encoding and byte subviews",
        &["subview-bounds"],
    ),
    core(
        "ecma.json.roundtrip",
        "JSON text survives direct and stored decoding",
        &["direct", "stored", "stored-unicode", "direct-unicode"],
    ),
    equiv(
        "node.timers.promises.value",
        "Timer result",
        NODE_API,
        TIMER_DOMAIN,
        &["retained-operation"],
    ),
    equiv(
        "node.timers.promises.observers",
        "Repeated timer observation",
        NODE_API,
        TIMER_DOMAIN,
        &["retained-operation"],
    ),
    equiv(
        "node.fs.readFile.utf8",
        "UTF-8 file read across retained and dynamic Unicode payloads",
        NODE_FS,
        "Retained async UTF-8 read interleaved with numeric evaluation, plus dynamic Unicode/NUL payloads of 0..48 scalar values and concurrent reads",
        &["retained-operation", "unicode-and-concurrent-read"],
    ),
    equiv(
        "node.fs.writeFile.text",
        "Text file round trip, overwrite and truncation",
        NODE_FS,
        "Retained JSON text round trip plus sequential UTF-8 writes testing file truncation and growth across arbitrary Unicode strings",
        &["retained-operation", "overwrite-and-truncate"],
    ),
    equiv(
        "node.fs.metadata",
        "File stat and directory listing across dynamic files and cleanup",
        NODE_FS,
        "Retained stat/readdir on fixture plus multi-file creation, exact byte-length stat verification, directory entry count and unlink cleanup",
        &["retained-operation", "multi-file-stat-and-unlink"],
    ),
    equiv(
        "node.fs.readFile.bytes",
        "Binary file read, buffer isolation and UTF-8 decoding",
        NODE_FS,
        "Retained binary read plus arbitrary 0..256-byte payloads, returned Uint8Array mutation isolation across repeated reads and TextDecoder verification",
        &["retained-operation", "binary-buffer-isolation"],
    ),
    equiv(
        "node.fs.writeFile.subviews",
        "Byte view and empty file round trip across arbitrary subarray slices",
        NODE_FS,
        "Retained subarray write plus arbitrary 0..128-byte buffers and subarray(start, end) slices, post-write view mutation isolation and empty subview truncation",
        &["retained-operation", "arbitrary-subview-slice"],
    ),
    equiv(
        "node.fs.readFile.recovery",
        "Repeated rejection across missing UTF-8, binary and stat calls followed by recovery",
        NODE_FS,
        "Retained rejected readFile plus repeated rejection observation across UTF-8 readFile, binary readFile and stat, followed by successful write/read recovery",
        &["retained-operation", "multi-op-rejection-recovery"],
    ),
    cli(
        "ecma.json.command",
        "CLI JSON serialization and schema-guided parsing",
    ),
    cli(
        "ecma.command.arrays",
        "CLI dense array manipulation and iteration",
    ),
    equiv(
        "ecma.command.sparse-arrays",
        "Sparse array presence, traversal, copying and serialization",
        "https://tc39.es/ecma262/multipage/indexed-collections.html",
        "Elisions and indexed deletion; explicit undefined; sparse numeric and reference arrays; exact output bytes and successful exit",
        &["standard-source"],
    ),
    equiv(
        "ecma.command.text",
        "CLI UTF-8 string formatting, UTF-16 code units and regular expressions",
        NODE_API,
        "Standard CLI source; charCodeAt over ASCII/NUL, BMP and supplementary text with omitted, undefined and IEEE-754 boundary positions; exact output bytes and successful exit",
        &["standard-source"],
    ),
    cli(
        "ecma.command.records",
        "CLI closed record layouts and nested objects",
    ),
    cli(
        "ecma.command.numbers",
        "CLI numeric formatting and boundary arithmetic",
    ),
    cli("ecma.command.web", "CLI Web API codecs and byte views"),
    equiv(
        "ecma.command.narrowing",
        "CLI typeof, tag and null narrowing across assignments, loops and handlers",
        "https://tc39.es/ecma262/#sec-typeof-operator",
        CLI_DOMAIN,
        &["standard-source"],
    ),
    cli(
        "ecma.command.temporal",
        "CLI ISO Temporal calendar and instant operations",
    ),
    cli(
        "web.console.routing",
        "Standard output and error stream routing via console",
    ),
    cli(
        "ecma.promise.all",
        "Concurrent Promise.all settlement ordering",
    ),
    cli(
        "web.performance.monotonic",
        "Monotonic clock reads via performance.now",
    ),
    cli(
        "ecma.date.clock",
        "Platform wall-clock reads and ISO Date formatting",
    ),
    cli(
        "web.crypto.random",
        "Cryptographic random byte fill via crypto.getRandomValues",
    ),
    cli(
        "node.process.environment",
        "Environment variable reads via process.env",
    ),
    equiv(
        "node.timers.promises.lifecycle",
        "Start, host release, completion, cancellation, observation and reuse",
        NODE_TIMERS,
        "Complete sequential lifecycle traces with controlled Node/WASI clocks; no claim of exhaustive async scheduling",
        &["settlement-and-reobserve", "cancel-and-reuse"],
    ),
    fault("wasi.execution.detect_value_fault"),
    fault("wasi.execution.detect_trace_fault"),
    fault("wasi.execution.detect_resource_fault"),
    equiv(
        "wasi.streams.incoming_bytes",
        "Owned WIT byte streams use Web readers and acknowledged Web writes",
        WHATWG_STREAMS,
        "Three calls per instance; input chunks of 8192 bytes; 64 KiB guest memory; exact output bytes, locking, cancellation and completion",
        STREAM_PARTITIONS,
    ),
    equiv(
        "web.streams.byob_reader",
        "BYOB readers transfer shared buffers and fill visible views directly",
        WHATWG_STREAMS,
        "Uint8Array destinations with offsets, alias detachment, buffer reuse, min:8192, short EOF, locking and cancellation; three calls per instance; exact output bytes within 64 KiB guest memory",
        STREAM_PARTITIONS,
    ),
    equiv(
        "web.fetch.bounded_body",
        "Headers-first Fetch, bounded byte consumption, completion failure and reuse",
        WHATWG_FETCH,
        FETCH_DOMAIN,
        FETCH_PARTITIONS,
    ),
    equiv(
        "web.fetch.bounded_byob_body",
        "BYOB bounded Fetch consumes directly into its final buffer",
        WHATWG_FETCH,
        FETCH_DOMAIN,
        FETCH_PARTITIONS,
    ),
    equiv(
        "ecma.uint8array.set",
        "Overlapping byte copies and atomic range rejection",
        "https://tc39.es/ecma262/#sec-%typedarray%.prototype.set",
        BYTES_DOMAIN,
        BYTES_PARTITIONS,
    ),
    equiv(
        "ecma.dataview.uint32",
        "Uint32 framing through a bounded DataView",
        "https://tc39.es/ecma262/#sec-dataview-objects",
        "Unaligned and empty views, shared typed-array writes, bounds and byte order",
        BYTES_PARTITIONS,
    ),
    equiv(
        "web.response.bytes",
        "Constructed response snapshot, body use and repeated consumption",
        WHATWG_FETCH,
        BYTES_DOMAIN,
        BYTES_PARTITIONS,
    ),
    equiv(
        "web.request.bytes",
        "Constructed request snapshot, body use and repeated consumption",
        WHATWG_FETCH,
        BYTES_DOMAIN,
        BYTES_PARTITIONS,
    ),
    equiv(
        "node.stream.writable_to_web",
        "Queued acknowledged writes, writer locks and closed-writer rejection",
        "https://nodejs.org/api/stream.html#streamwritabletowebstreamwritable",
        "stdout/stderr Uint8Array writes and subviews; two queued writes, repeat observation, close, rejected released-writer use and a new stdio wrapper; exact output bytes",
        &["empty", "binary-and-close"],
    ),
    equiv(
        "node.timers.callbacks.named",
        "Named callback timers, cancellation and forwarded arguments",
        NODE_TIMERS,
        "Both module spellings, an import alias and byte-valued callback arguments",
        &["timers", "node:timers"],
    ),
    reject(
        "node.timers.callbacks.namespace",
        "Explicit compiler rejection of timer namespace members",
        NODE_TIMERS,
        "Namespace import used as a callback timer receiver",
        &["namespace"],
    ),
    reject(
        "ecma.module.unawaited_initialization",
        "Explicit compiler rejection of module initialization that entry points cannot await",
        "https://tc39.es/ecma262/#sec-moduleevaluation",
        "Synchronous top-level statements of a module with exported functions that start asynchronous work without awaiting it",
        &["unawaited-async-call"],
    ),
    equiv(
        "node.fs.directories",
        "Directory creation, listing, metadata and deletion",
        NODE_FS,
        "One directory and binary file; exact size, one listed entry, cleanup and reuse across the numeric boundary corpus",
        &["empty-file", "binary-file"],
    ),
    equiv(
        "web.textdecoder.streaming",
        "Fatal UTF-8 decoding across byte boundaries",
        "https://encoding.spec.whatwg.org/",
        "Fatal UTF-8, one-byte chunks, final flush; compare successful prefix bytes and normalized decoding failure",
        &[
            "empty",
            "bom-and-split-scalar",
            "invalid-sequence",
            "incomplete-sequence",
        ],
    ),
    equiv(
        "web.headers.multimap",
        "Header name folding, joined duplicates and mutations",
        WHATWG_FETCH,
        "ASCII values, case-insensitive names, whitespace trimming, append/get/set/has/delete",
        &["empty-value", "trim-and-duplicates"],
    ),
    equiv(
        "ecma.temporal.iso",
        "Typed ISO Instant, PlainDate and PlainDateTime values",
        "https://tc39.es/proposal-temporal/",
        "Integral epoch milliseconds in +/-8640000000000000, ISO formatting, leap-day addition and the ISO 8601 calendarId",
        &[
            "negative-millisecond",
            "epoch",
            "lower-limit",
            "upper-limit",
        ],
    ),
    equiv(
        "node.process.exit_status",
        "Explicit failure status and immediate process exit",
        "https://nodejs.org/api/process.html",
        "Exit status 1 via process.exitCode or process.exit; exact preceding output and unreachable following output",
        &["exit-code", "immediate-exit"],
    ),
    equiv(
        "ecma.promise.all_settled",
        "Fulfilled and rejected results retain their status and payload",
        PROMISE_SPEC,
        COMBINATOR_DOMAIN,
        &["zero", "negative"],
    ),
    equiv(
        "ecma.promise.race",
        "Already-settled contenders retain input order and repeated outcomes",
        PROMISE_SPEC,
        COMBINATOR_DOMAIN,
        &["zero", "negative"],
    ),
    equiv(
        "ecma.error.values",
        "Error messages, causes and identity across promise rejection and rethrow",
        "https://tc39.es/ecma262/#sec-error-objects",
        "TypeError with a string message and cause; repeated awaits, all/allSettled/race, rethrow and finally; hidden built-in fields; Unicode messages of 0..32 scalar values",
        &["empty-message", "nul-and-unicode"],
    ),
    catalog_entry(
        "ecma.array.inferred_storage",
        "Fresh empty arrays infer uniform or finite mixed elements from visible writes",
        "https://www.typescriptlang.org/docs/handbook/type-inference.html",
        "Up to seven writes and four aliases over numbers, strings, booleans, records and nested arrays; annotations versus alias chains, branches, loops, temporaries, helper-return chains and indexed appends; three calls compare exact serialized contents and evaluation traces; finite mixed writes and returned arrays preserve values; unresolved writes remain rejected through temporaries and control flow; conservative escape boundaries",
        &[
            "uniform-records-and-aliases",
            "dependent-arrays",
            "mixed-index-writes",
            "mixed-alias",
            "mixed-branch",
            "returned-alias",
            "mixed-function-write",
            "string-key-write",
            "conflicting-conditional-field",
            "conflicting-nested-array",
            "unknown-call",
            "closure-capture",
            "reassigned-alias",
            "stored-alias",
            "mutable-element-alias",
            "no-writes",
            "conditional-temporary",
            "number-writes",
            "string-writes",
            "boolean-writes",
            "record-writes",
            "nested-array-writes",
        ],
        &[
            ExpectedOutcome::EquivalentExecution,
            ExpectedOutcome::CompilerRejection,
        ],
    ),
    catalog_entry(
        "ecma.object.inferred_storage",
        "Fresh empty dictionaries share array inference for visible writes and aliases",
        "https://tc39.es/ecma262/#sec-object-type",
        "Uniform string-keyed writes over numbers, strings, booleans, records and arrays; annotation versus alias, branch, loop and temporary forms; computed keys, assignment values, absent reads, reserved array property names, nested typed JSON residue; unknown sources and escaping mutations remain rejected",
        &[
            "computed-keys-and-json-residue",
            "conflicting-alias",
            "conflicting-branch",
            "numeric-key",
            "unknown-call",
            "reassigned-alias",
            "stored-alias",
            "returned-alias",
            "mutable-element-alias",
            "open-json-source",
            "number-writes",
            "string-writes",
            "boolean-writes",
            "record-writes",
            "nested-array-writes",
        ],
        &[
            ExpectedOutcome::EquivalentExecution,
            ExpectedOutcome::CompilerRejection,
        ],
    ),
    equiv(
        "wasi.http.handler",
        "Native HTTP ABI with WIT-generated request and response declarations",
        "https://github.com/WebAssembly/wasi-http",
        "PUT requests with binary bodies up to 64 KiB, duplicate headers, statuses 200/201/404; three exchanges reuse one component instance",
        &["empty", "binary-and-duplicates", "request-response-limit"],
    ),
    scaling(
        "wasi.execution.scaling.retained_records",
        "Pushing n records into a local array and reading them back costs O(n) guest fuel",
    ),
    scaling(
        "wasi.execution.scaling.number_push",
        "Pushing n numbers into a dense array and summing them costs O(n) guest fuel",
    ),
    scaling(
        "wasi.execution.scaling.temporary_records",
        "One temporary record per loop iteration costs O(n) guest fuel in total",
    ),
    scaling(
        "wasi.execution.scaling.string_append",
        "Appending one character n times costs at most n·log n guest fuel",
    ),
    scaling(
        "wasi.execution.scaling.collection_churn",
        "Abandoned Set/Map iterators and insert-delete churn cost O(n) guest fuel",
    ),
    equiv(
        "ecma.aggregate.specialization",
        "Orthogonal aggregate layout specialization across recursive shapes, exact 64-bit parseInt, origin scaling, and WIT boundaries",
        ECMA262,
        "Product of flat/nested/self-recursive/mutually-recursive/array-bearing records, multi-field/list/option WIT exports, float/NaN/-0.0/int-arith/wide-parseInt value sources, branch joins, shared helpers, array concat/spread, and >32-origin summarization",
        &[
            "persistent-record-map-aliases",
            "self-recursive-wide-parseint-and-float",
            "nested-record-option-boundary-special-values-and-overflow",
        ],
    ),
    equiv(
        "ecma.collection.iterators",
        "Live Set and Map iterators across active, paused, and abandoned lifetimes under delete compaction and clear",
        "https://tc39.es/ecma262/#sec-keyed-collections",
        "Orthogonal state-machine sequences of NewIter, partial Step and ForEach with in-body mutations, Insert/InsertBatch, Delete/DeleteBatch across the 32-tombstone compaction threshold, Clear, and abandoned cursor churn over Set.values, Set.entries, Map.keys, Map.values, and Map.entries; Node is the oracle",
        &[
            "set-paused-clear-foreach-and-reinsert",
            "map-entries-compaction-and-abandoned-cursor-churn",
        ],
    ),
];

/// The catalog entry of contract `id`.
#[cfg(not(conformance_doc_bin))]
pub(crate) fn entry(id: &str) -> Option<&'static CatalogEntry> {
    ENTRIES.iter().find(|entry| entry.id == id)
}

/// Contract id prefixes and the catalog headings they appear under, in page order.
const NAMESPACES: [(&str, &str); 4] = [
    ("ecma", "ECMAScript"),
    ("web", "Web Platform"),
    ("node", "Node.js"),
    ("wasi", "WASI 0.3 Host"),
];

/// The position in [NAMESPACES] of the namespace contract `id` belongs to.
pub(crate) fn namespace(id: &str) -> Option<usize> {
    let prefix = id.split_once('.')?.0;
    NAMESPACES.iter().position(|(name, _)| *name == prefix)
}

/// The handbook's catalog page: a summary line and one table per namespace.
pub(crate) fn render_markdown(all: &[CatalogEntry]) -> Result<String, String> {
    let mut tables: [String; NAMESPACES.len()] = Default::default();
    let (mut passing_count, mut deferred_count, mut pending_count) = (0, 0, 0);
    for entry in all {
        if passing(entry.id) {
            passing_count += 1;
        } else if deferred(entry.id).is_some() {
            deferred_count += 1;
        } else {
            pending_count += 1;
        }
        let table = namespace(entry.id)
            .map(|index| &mut tables[index])
            .ok_or_else(|| format!("unknown contract namespace in {}", entry.id))?;
        // Only a URL specification becomes a link; others (internal cost bounds) stay plain.
        if entry.specification.starts_with("https://") {
            let _ = write!(table, "| [`{}`]({}) |", entry.id, entry.specification);
        } else {
            let _ = write!(table, "| `{}` |", entry.id);
        }
        let _ = write!(table, " {} | ", entry.description);
        for (index, outcome) in entry.expected_outcomes.iter().enumerate() {
            let separator = if index == 0 { "" } else { ", " };
            let _ = write!(table, "{separator}{}", outcome.label());
        }
        table.push_str(" | ");
        for (index, partition) in entry.partitions.iter().enumerate() {
            let separator = if index == 0 { "" } else { ", " };
            let _ = write!(table, "{separator}`{partition}`");
        }
        let _ = writeln!(table, " | {} |", entry.domain);
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "The registry holds **{}** capability contracts: **{passing_count}** passing, **{deferred_count}** deferred, **{pending_count}** pending.\n",
        all.len(),
    );
    for ((_, heading), rows) in NAMESPACES.iter().zip(&tables) {
        let _ = writeln!(out, "### {heading}\n");
        let _ = writeln!(
            out,
            "| Contract | Capability | Outcome | Partitions | Domain |"
        );
        let _ = writeln!(out, "| :--- | :--- | :--- | :--- | :--- |");
        out.push_str(rows);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(conformance_doc_bin)]
fn main() {
    match render_markdown(ENTRIES) {
        Ok(markdown) => print!("{markdown}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
