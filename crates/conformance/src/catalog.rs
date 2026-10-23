//! Static conformance catalog verified against executable contract definitions.
#![allow(unexpected_cfgs)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(conformance_doc_bin), derive(serde::Serialize))]
pub struct CatalogEntry {
    pub id: &'static str,
    pub(crate) description: &'static str,
    pub(crate) specification: &'static str,
    pub(crate) domain: &'static str,
    pub partitions: Vec<&'static str>,
    pub(crate) expected_outcomes: Vec<ExpectedOutcome>,
}

pub(crate) fn entries() -> Vec<CatalogEntry> {
    vec![
        CatalogEntry {
            id: "ecma.number.arithmetic",
            description: "IEEE arithmetic and evaluation order",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec![
                "signed-zero",
                "division",
                "trace",
                "call",
                "record-field-update",
                "byte-alias",
                "checked-json",
                "handled-throw",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.math.rounding",
            description: "Numeric Math operations",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec!["floor", "ceil", "trunc", "abs", "round"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.string.operations",
            description: "String search, casing, slicing and length",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec![
                "unicode-and-nul",
                "search",
                "lower",
                "upper",
                "slice",
                "char-at",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.array.dense",
            description: "Dense numeric arrays preserve values and effects",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec![
                "index",
                "branch-true",
                "branch-false",
                "first-index",
                "array-effects",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.object.records",
            description: "Record fields preserve values and effects",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec!["field-order"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.date.epoch",
            description: "Date TimeClip and epoch reads",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec!["timeclip", "nested-codecs"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.uint8array.conversion",
            description: "Byte conversion and indexed access",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec!["wrapping", "byte-input", "byte-effects"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.textencoder.subviews",
            description: "UTF-8 encoding and byte subviews",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec!["subview-bounds"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.json.roundtrip",
            description: "JSON text survives direct and stored decoding",
            specification: "https://tc39.es/ecma262/",
            domain: "Bounded typed functions, control flow, closed aggregates, byte aliases, JSON conversion and exceptions over the IEEE-754 boundary corpus; BMP strings; direct/local/branch/loop/inert-exception forms and overlapping timer completion schedules",
            partitions: vec!["direct", "stored", "stored-unicode", "direct-unicode"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.timers.promises.value",
            description: "Timer result",
            specification: "https://nodejs.org/api/",
            domain: "Retained async operation interleaved with typed numeric evaluation; instance reused across boundary inputs",
            partitions: vec!["retained-operation"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.timers.promises.observers",
            description: "Repeated timer observation",
            specification: "https://nodejs.org/api/",
            domain: "Retained async operation interleaved with typed numeric evaluation; instance reused across boundary inputs",
            partitions: vec!["retained-operation"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.fs.readFile.utf8",
            description: "UTF-8 file read across retained and dynamic Unicode payloads",
            specification: "https://nodejs.org/api/fs.html",
            domain: "Retained async UTF-8 read interleaved with numeric evaluation, plus dynamic Unicode/NUL payloads of 0..48 scalar values and concurrent reads",
            partitions: vec!["retained-operation", "unicode-and-concurrent-read"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.fs.writeFile.text",
            description: "Text file round trip, overwrite and truncation",
            specification: "https://nodejs.org/api/fs.html",
            domain: "Retained JSON text round trip plus sequential UTF-8 writes testing file truncation and growth across arbitrary Unicode strings",
            partitions: vec!["retained-operation", "overwrite-and-truncate"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.fs.metadata",
            description: "File stat and directory listing across dynamic files and cleanup",
            specification: "https://nodejs.org/api/fs.html",
            domain: "Retained stat/readdir on fixture plus multi-file creation, exact byte-length stat verification, directory entry count and unlink cleanup",
            partitions: vec!["retained-operation", "multi-file-stat-and-unlink"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.fs.readFile.bytes",
            description: "Binary file read, buffer isolation and UTF-8 decoding",
            specification: "https://nodejs.org/api/fs.html",
            domain: "Retained binary read plus arbitrary 0..256-byte payloads, returned Uint8Array mutation isolation across repeated reads and TextDecoder verification",
            partitions: vec!["retained-operation", "binary-buffer-isolation"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.fs.writeFile.subviews",
            description: "Byte view and empty file round trip across arbitrary subarray slices",
            specification: "https://nodejs.org/api/fs.html",
            domain: "Retained subarray write plus arbitrary 0..128-byte buffers and subarray(start, end) slices, post-write view mutation isolation and empty subview truncation",
            partitions: vec!["retained-operation", "arbitrary-subview-slice"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.fs.readFile.recovery",
            description: "Repeated rejection across missing UTF-8, binary and stat calls followed by recovery",
            specification: "https://nodejs.org/api/fs.html",
            domain: "Retained rejected readFile plus repeated rejection observation across UTF-8 readFile, binary readFile and stat, followed by successful write/read recovery",
            partitions: vec!["retained-operation", "multi-op-rejection-recovery"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.json.command",
            description: "CLI JSON serialization and schema-guided parsing",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.command.arrays",
            description: "CLI dense array manipulation and iteration",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.command.text",
            description: "CLI UTF-8 string formatting and regular expressions",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.command.records",
            description: "CLI closed record layouts and nested objects",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.command.numbers",
            description: "CLI numeric formatting and boundary arithmetic",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.command.web",
            description: "CLI Web API codecs and byte views",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.command.temporal",
            description: "CLI ISO Temporal calendar and instant operations",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.console.routing",
            description: "Standard output and error stream routing via console",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.promise.all",
            description: "Concurrent Promise.all settlement ordering",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.performance.monotonic",
            description: "Monotonic clock reads via performance.now",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.date.clock",
            description: "Platform wall-clock reads and ISO Date formatting",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.crypto.random",
            description: "Cryptographic random byte fill via crypto.getRandomValues",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.process.environment",
            description: "Environment variable reads via process.env",
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            partitions: vec!["standard-source"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.timers.promises.lifecycle",
            description: "Start, host release, completion, cancellation, observation and reuse",
            specification: "https://nodejs.org/api/timers.html",
            domain: "Complete sequential lifecycle traces with controlled Node/WASI clocks; no claim of exhaustive async scheduling",
            partitions: vec!["settlement-and-reobserve", "cancel-and-reuse"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "wasi.execution.detect_value_fault",
            description: "Injected faults must be distinguished from the unchanged program",
            specification: "https://component-model.bytecodealliance.org/",
            domain: "Value corruption, omitted effects, and retained host resources; only the component adapter is faulted",
            partitions: vec!["injected-fault"],
            expected_outcomes: vec![ExpectedOutcome::InjectedFaultDetection],
        },
        CatalogEntry {
            id: "wasi.execution.detect_trace_fault",
            description: "Injected faults must be distinguished from the unchanged program",
            specification: "https://component-model.bytecodealliance.org/",
            domain: "Value corruption, omitted effects, and retained host resources; only the component adapter is faulted",
            partitions: vec!["injected-fault"],
            expected_outcomes: vec![ExpectedOutcome::InjectedFaultDetection],
        },
        CatalogEntry {
            id: "wasi.execution.detect_resource_fault",
            description: "Injected faults must be distinguished from the unchanged program",
            specification: "https://component-model.bytecodealliance.org/",
            domain: "Value corruption, omitted effects, and retained host resources; only the component adapter is faulted",
            partitions: vec!["injected-fault"],
            expected_outcomes: vec![ExpectedOutcome::InjectedFaultDetection],
        },
        CatalogEntry {
            id: "wasi.streams.incoming_bytes",
            description: "Owned WIT byte streams use Web readers and acknowledged Web writes",
            specification: "https://streams.spec.whatwg.org/",
            domain: "Three calls per instance; input chunks of 8192 bytes; 64 KiB guest memory; exact output bytes, locking, cancellation and completion",
            partitions: vec![
                "empty",
                "byte-boundary",
                "transfer-boundary",
                "beyond-guest-memory",
                "cancel-and-reuse",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.streams.byob_reader",
            description: "BYOB readers transfer shared buffers and fill visible views directly",
            specification: "https://streams.spec.whatwg.org/",
            domain: "Uint8Array destinations with offsets, alias detachment, buffer reuse, min:8192, short EOF, locking and cancellation; three calls per instance; exact output bytes within 64 KiB guest memory",
            partitions: vec![
                "empty",
                "byte-boundary",
                "transfer-boundary",
                "beyond-guest-memory",
                "cancel-and-reuse",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.fetch.bounded_body",
            description: "Headers-first Fetch, bounded byte consumption, completion failure and reuse",
            specification: "https://fetch.spec.whatwg.org/",
            domain: "GET loopback HTTP; 200 and 404; binary bodies, explicit body cap and producer truncation; three calls per instance",
            partitions: vec![
                "empty",
                "binary-exact-limit",
                "non-success-status",
                "overflow-cancellation",
                "completion-error",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.fetch.bounded_byob_body",
            description: "BYOB bounded Fetch consumes directly into its final buffer",
            specification: "https://fetch.spec.whatwg.org/",
            domain: "GET loopback HTTP; 200 and 404; binary bodies, explicit body cap and producer truncation; three calls per instance",
            partitions: vec![
                "empty",
                "binary-exact-limit",
                "non-success-status",
                "overflow-cancellation",
                "completion-error",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.uint8array.set",
            description: "Overlapping byte copies and atomic range rejection",
            specification: "https://tc39.es/ecma262/#sec-%typedarray%.prototype.set",
            domain: "Uint8Array sources of 0..256 bytes; each source evaluated over every IEEE-754 boundary input; byte reads checked explicitly",
            partitions: vec!["empty", "binary-subview"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.response.bytes",
            description: "Constructed response snapshot, body use and repeated consumption",
            specification: "https://fetch.spec.whatwg.org/",
            domain: "Uint8Array sources of 0..256 bytes; each source evaluated over every IEEE-754 boundary input; byte reads checked explicitly",
            partitions: vec!["empty", "binary-subview"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.request.bytes",
            description: "Constructed request snapshot, body use and repeated consumption",
            specification: "https://fetch.spec.whatwg.org/",
            domain: "Uint8Array sources of 0..256 bytes; each source evaluated over every IEEE-754 boundary input; byte reads checked explicitly",
            partitions: vec!["empty", "binary-subview"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.stream.writable_to_web",
            description: "Queued acknowledged writes, writer locks and closed-writer rejection",
            specification: "https://nodejs.org/api/stream.html#streamwritabletowebstreamwritable",
            domain: "stdout/stderr Uint8Array writes and subviews; two queued writes, repeat observation, close, rejected released-writer use and a new stdio wrapper; exact output bytes",
            partitions: vec!["empty", "binary-and-close"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.timers.callbacks.unsupported",
            description: "Explicit compiler rejection of callback timers",
            specification: "https://nodejs.org/api/timers.html",
            domain: "Callback timers are outside the supported source domain",
            partitions: vec!["callback-timer"],
            expected_outcomes: vec![ExpectedOutcome::CompilerRejection],
        },
        CatalogEntry {
            id: "node.fs.directories",
            description: "Directory creation, listing, metadata and deletion",
            specification: "https://nodejs.org/api/fs.html",
            domain: "One directory and binary file; exact size, one listed entry, cleanup and reuse across the numeric boundary corpus",
            partitions: vec!["empty-file", "binary-file"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.textdecoder.streaming",
            description: "Fatal UTF-8 decoding across byte boundaries",
            specification: "https://encoding.spec.whatwg.org/",
            domain: "Fatal UTF-8, one-byte chunks, final flush; compare successful prefix bytes and normalized decoding failure",
            partitions: vec![
                "empty",
                "bom-and-split-scalar",
                "invalid-sequence",
                "incomplete-sequence",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "web.headers.multimap",
            description: "Header name folding, joined duplicates and mutations",
            specification: "https://fetch.spec.whatwg.org/",
            domain: "ASCII values, case-insensitive names, whitespace trimming, append/get/set/has/delete",
            partitions: vec!["empty-value", "trim-and-duplicates"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.temporal.iso",
            description: "Typed ISO Instant, PlainDate and PlainDateTime values",
            specification: "https://tc39.es/proposal-temporal/",
            domain: "Integral epoch milliseconds in +/-8640000000000000, ISO formatting and leap-day addition; pinned Node Temporal enabled explicitly",
            partitions: vec![
                "negative-millisecond",
                "epoch",
                "lower-limit",
                "upper-limit",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "node.process.exit_status",
            description: "Explicit failure status and immediate process exit",
            specification: "https://nodejs.org/api/process.html",
            domain: "Exit status 1 via process.exitCode or process.exit; exact preceding output and unreachable following output",
            partitions: vec!["exit-code", "immediate-exit"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.promise.all_settled",
            description: "Fulfilled and rejected results retain their status and payload",
            specification: "https://tc39.es/ecma262/#sec-promise-objects",
            domain: "Two numeric tasks; one fulfillment and one rejection for allSettled, two already-completed contenders for race; repeated component calls and IEEE-754 inputs",
            partitions: vec!["zero", "negative"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.promise.race",
            description: "Already-settled contenders retain input order and repeated outcomes",
            specification: "https://tc39.es/ecma262/#sec-promise-objects",
            domain: "Two numeric tasks; one fulfillment and one rejection for allSettled, two already-completed contenders for race; repeated component calls and IEEE-754 inputs",
            partitions: vec!["zero", "negative"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.error.values",
            description: "Error messages, causes and identity across promise rejection and rethrow",
            specification: "https://tc39.es/ecma262/#sec-error-objects",
            domain: "TypeError with a string message and cause; repeated awaits, all/allSettled/race, rethrow and finally; hidden built-in fields; Unicode messages of 0..32 scalar values",
            partitions: vec!["empty-message", "nul-and-unicode"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "wasi.http.handler",
            description: "Native HTTP ABI with WIT-generated request and response declarations",
            specification: "https://github.com/WebAssembly/wasi-http",
            domain: "PUT requests with binary bodies up to 64 KiB, duplicate headers, statuses 200/201/404; three exchanges reuse one component instance",
            partitions: vec!["empty", "binary-and-duplicates", "request-response-limit"],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "wasi.execution.scaling.retained_records",
            description: "Pushing n records into a local array and reading them back costs O(n) guest fuel",
            specification: "TODOs.md P0.1 (implementation cost bound, not an ECMAScript requirement)",
            domain: "Warm-up n, n, and 4n calls on one instance; n in 32..=128 generated, 128 directed; values equal Node, fuel ratio of the last two calls at most 6",
            partitions: vec!["n-128"],
            expected_outcomes: vec![ExpectedOutcome::BoundedGrowth],
        },
        CatalogEntry {
            id: "wasi.execution.scaling.number_push",
            description: "Pushing n numbers into a dense array and summing them costs O(n) guest fuel",
            specification: "TODOs.md P0.1 (implementation cost bound, not an ECMAScript requirement)",
            domain: "Warm-up n, n, and 4n calls on one instance; n in 32..=128 generated, 128 directed; values equal Node, fuel ratio of the last two calls at most 6",
            partitions: vec!["n-128"],
            expected_outcomes: vec![ExpectedOutcome::BoundedGrowth],
        },
        CatalogEntry {
            id: "wasi.execution.scaling.temporary_records",
            description: "One temporary record per loop iteration costs O(n) guest fuel in total",
            specification: "TODOs.md P0.1 (implementation cost bound, not an ECMAScript requirement)",
            domain: "Warm-up n, n, and 4n calls on one instance; n in 32..=128 generated, 128 directed; values equal Node, fuel ratio of the last two calls at most 6",
            partitions: vec!["n-128"],
            expected_outcomes: vec![ExpectedOutcome::BoundedGrowth],
        },
        CatalogEntry {
            id: "wasi.execution.scaling.string_append",
            description: "Appending one character n times costs at most n·log n guest fuel",
            specification: "TODOs.md P0.1 (implementation cost bound, not an ECMAScript requirement)",
            domain: "Warm-up n, n, and 4n calls on one instance; n in 32..=128 generated, 128 directed; values equal Node, fuel ratio of the last two calls at most 6",
            partitions: vec!["n-128"],
            expected_outcomes: vec![ExpectedOutcome::BoundedGrowth],
        },
        CatalogEntry {
            id: "wasi.execution.scaling.collection_churn",
            description: "Abandoned Set/Map iterators and insert-delete churn cost O(n) guest fuel",
            specification: "TODOs.md P0.1 (implementation cost bound, not an ECMAScript requirement)",
            domain: "Warm-up n, n, and 4n calls on one instance; n in 32..=128 generated, 128 directed; values equal Node, fuel ratio of the last two calls at most 6",
            partitions: vec!["n-128"],
            expected_outcomes: vec![ExpectedOutcome::BoundedGrowth],
        },
        CatalogEntry {
            id: "ecma.aggregate.specialization",
            description: "Orthogonal aggregate layout specialization across recursive shapes, exact 64-bit parseInt, origin scaling, and WIT boundaries",
            specification: "https://tc39.es/ecma262/",
            domain: "Product of flat/nested/self-recursive/mutually-recursive/array-bearing records, multi-field/list/option WIT exports, float/NaN/-0.0/int-arith/wide-parseInt value sources, branch joins, shared helpers, array concat/spread, and >32-origin summarization",
            partitions: vec![
                "self-recursive-wide-parseint-and-float",
                "nested-record-option-boundary-special-values-and-overflow",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
        CatalogEntry {
            id: "ecma.collection.iterators",
            description: "Live Set and Map iterators across active, paused, and abandoned lifetimes under delete compaction and clear",
            specification: "https://tc39.es/ecma262/#sec-keyed-collections",
            domain: "Orthogonal state-machine sequences of NewIter, partial Step and ForEach with in-body mutations, Insert/InsertBatch, Delete/DeleteBatch across the 32-tombstone compaction threshold, Clear, and abandoned cursor churn over Set.values, Set.entries, Map.keys, Map.values, and Map.entries; Node is the oracle",
            partitions: vec![
                "set-paused-clear-foreach-and-reinsert",
                "map-entries-compaction-and-abandoned-cursor-churn",
            ],
            expected_outcomes: vec![ExpectedOutcome::EquivalentExecution],
        },
    ]
}

pub(crate) fn render_markdown(all: &[CatalogEntry]) -> String {
    let mut ecma = Vec::new();
    let mut web = Vec::new();
    let mut node = Vec::new();
    let mut wasi = Vec::new();
    let mut passing_count = 0usize;
    let mut deferred_count = 0usize;
    let mut pending_count = 0usize;

    for entry in all {
        let status = if passing(entry.id) {
            passing_count += 1;
            "Passing".to_string()
        } else if let Some(reason) = deferred(entry.id) {
            deferred_count += 1;
            format!("Deferred ({reason})")
        } else {
            pending_count += 1;
            "Pending".to_string()
        };
        let outcomes = entry
            .expected_outcomes
            .iter()
            .map(|o| o.label())
            .collect::<Vec<_>>()
            .join(", ");
        let partitions = entry
            .partitions
            .iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let row = format!(
            "| [`{}`]({}) | {} | {} | {} | {} | {} |\n",
            entry.id,
            entry.specification,
            entry.description,
            outcomes,
            partitions,
            entry.domain,
            status
        );
        match entry.id.split_once('.').map(|(prefix, _)| prefix) {
            Some("ecma") => ecma.push(row),
            Some("web") => web.push(row),
            Some("node") => node.push(row),
            Some("wasi") => wasi.push(row),
            _ => {}
        }
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "The executable registry tracks **{}** capability contracts (**{}** passing, **{}** deferred, **{}** pending).\n",
        all.len(),
        passing_count,
        deferred_count,
        pending_count
    );

    for (heading, summary, rows) in [
        (
            "ECMAScript",
            "Core language semantics, numeric arithmetic, aggregates, collections, strings, JSON, dates, Temporal, promises, and CLI execution equivalence.",
            &ecma,
        ),
        (
            "Web Platform",
            "Standard web interfaces including Fetch, ReadableStream, BYOB readers, Headers, Request, Response, TextEncoder, TextDecoder, Web Crypto, monotonic clocks, and Console streams.",
            &web,
        ),
        (
            "Node Compatibility",
            "Node.js built-in module contracts across `node:fs/promises`, `node:timers/promises`, stream adapters, and `process` environment/exit handling.",
            &node,
        ),
        (
            "WASI 0.3 Host",
            "Component Model boundary contracts, `wasi:http/handler@0.3.0`, native P3 byte streams, linear fuel scaling bounds, and fault-injection detection.",
            &wasi,
        ),
    ] {
        let _ = writeln!(out, "### {heading}\n\n{summary}\n");
        let _ = writeln!(
            out,
            "| Contract | Capability | Outcome | Partitions | Domain | Status |"
        );
        let _ = writeln!(out, "| :--- | :--- | :--- | :--- | :--- | :--- |");
        for row in rows {
            out.push_str(row);
        }
        out.push('\n');
    }

    out
}

#[cfg(conformance_doc_bin)]
fn main() {
    print!("{}", render_markdown(&entries()));
}
