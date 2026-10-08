//! Native codes are meaningful only together with the subsystem that produced them.
use super::super::{resolve::ResolvedContract, strings::StringPool};
use waffle::{MemoryData, MemorySegment};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Domain {
    Runtime,
    Encoding,
    Date,
    Temporal,
    Filesystem,
    Random,
    CodePoint,
    Binding,
    Body,
    Stream,
    Output,
    Fetch,
}

const DOMAINS: &[Domain] = &[
    Domain::Runtime,
    Domain::Encoding,
    Domain::Date,
    Domain::Temporal,
    Domain::Filesystem,
    Domain::Random,
    Domain::CodePoint,
    Domain::Binding,
    Domain::Body,
    Domain::Stream,
    Domain::Output,
    Domain::Fetch,
];
type Description = (&'static str, &'static str);
impl Domain {
    pub(crate) fn address(self, base: u32) -> u32 {
        base + self as u32 * 36
    }
    pub(crate) fn context(self) -> Option<&'static str> {
        match self {
            Self::Filesystem => Some("path"),
            Self::Fetch => Some("url"),
            _ => None,
        }
    }
    fn default(self) -> Description {
        match self {
            Self::Runtime => ("Error", "Native operation failed"),
            Self::Encoding => ("TypeError", "Invalid UTF-8 data"),
            Self::Date => ("RangeError", "Invalid time value"),
            Self::Temporal => ("RangeError", "Invalid Temporal value"),
            Self::Filesystem => ("Error", "Filesystem operation failed"),
            Self::Random => ("TypeMismatchError", "Expected an integer typed array"),
            Self::CodePoint => ("RangeError", "Invalid Unicode code point"),
            Self::Binding => (
                "ReferenceError",
                "Cannot access binding before initialization",
            ),
            Self::Body => ("TypeError", "HTTP body read failed"),
            Self::Stream => ("TypeError", "Stream operation failed"),
            Self::Output => ("Error", "Output stream failed"),
            Self::Fetch => ("TypeError", "fetch failed"),
        }
    }
    fn cases(self) -> &'static [(u32, Description)] {
        match self {
            Self::Runtime => &[
                (1, ("RangeError", "Value is outside the supported range")),
                (12, ("TypeError", "Invalid argument or receiver")),
            ],
            Self::Encoding => &[(1, ("RangeError", "Unsupported text encoding"))],
            Self::Temporal => &[
                (1, ("RangeError", "Invalid ISO date or time")),
                (
                    2,
                    ("RangeError", "Date or time is outside the supported range"),
                ),
                (3, ("RangeError", "Unsupported date or time annotation")),
                (4, ("RangeError", "Date or time storage limit exceeded")),
            ],
            Self::Random => &[(
                2,
                (
                    "QuotaExceededError",
                    "Random byte request exceeds 65536 bytes",
                ),
            )],
            Self::Body => &[(12, ("TypeError", "Body is already used or locked"))],
            Self::Fetch => &[(12, ("TypeError", "Invalid fetch request"))],
            Self::Stream => &[(
                12,
                ("TypeError", "Stream is locked, released, or already in use"),
            )],
            Self::Output => &[
                (1, ("Error", "Output stream I/O error")),
                (2, ("Error", "Invalid output byte sequence")),
                (3, ("Error", "Output stream closed before write completed")),
                (12, ("TypeError", "Writer is released or stream is closed")),
            ],
            _ => &[],
        }
    }
}

pub(crate) fn strings() -> impl Iterator<Item = &'static str> {
    DOMAINS
        .iter()
        .flat_map(|domain| {
            std::iter::once(domain.default())
                .chain(domain.cases().iter().map(|(_, description)| *description))
        })
        .flat_map(|(name, message)| [name, message])
        .chain(["AbortError", "The operation was aborted"])
}

/// Static records: default (kind, name, message), cases (pointer, count),
/// categories (code base, pointer, count), and optional context key.
pub(crate) fn emit_data(
    pool: &StringPool,
    contract: &ResolvedContract,
    memory: &mut MemoryData,
) -> u32 {
    fn append(words: &mut Vec<u32>, base: u32, values: impl IntoIterator<Item = u32>) -> [u32; 2] {
        let start = words.len();
        words.extend(values);
        [base + 4 * start as u32, (words.len() - start) as u32]
    }
    let description = |(name, message): Description| {
        [
            super::NAMES
                .iter()
                .position(|candidate| *candidate == name)
                .unwrap_or(0) as u32,
            pool.get(name).unwrap(),
            pool.get(message).unwrap(),
        ]
    };
    let base = pool.next_free_address();
    let mut words = vec![0; DOMAINS.len() * 9];
    let categories = ["filesystem", "http"].map(|package| {
        append(
            &mut words,
            base,
            contract
                .wit
                .iter()
                .flat_map(|wit| wit.error_names(package))
                .map(|name| pool.get(name).unwrap()),
        )
    });
    for &domain in DOMAINS {
        let cases = domain.cases().iter().copied().chain(
            (domain != Domain::Filesystem)
                .then_some((20, ("AbortError", "The operation was aborted"))),
        );
        let [cases, count] = append(
            &mut words,
            base,
            cases.flat_map(|(code, desc)| {
                let [kind, name, message] = description(desc);
                [code, kind, name, message]
            }),
        );
        let (code_base, [names, names_count]) = match domain {
            Domain::Filesystem => (1, categories[0]),
            Domain::Fetch | Domain::Body | Domain::Stream => (100, categories[1]),
            _ => (0, [0, 0]),
        };
        let start = domain as usize * 9;
        words[start..start + 3].copy_from_slice(&description(domain.default()));
        words[start + 3..start + 9].copy_from_slice(&[
            cases,
            count / 4,
            code_base,
            names,
            names_count,
            domain.context().map_or(0, |key| pool.get(key).unwrap()),
        ]);
    }
    let end = base + 4 * words.len() as u32;
    memory.initial_pages = memory.initial_pages.max(end.div_ceil(65_536) as usize);
    memory.segments.push(MemorySegment {
        offset: base as usize,
        data: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    });
    end
}
