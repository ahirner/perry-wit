//! Compiler-emitted regex tables. Invalid tables indicate a compiler bug and trap.
pub const MAX_CAPTURES: usize = 8;
pub const MAX_STATES: usize = 256;
pub const MAX_THREADS: usize = 24;
pub const MAX_PENDING: usize = 64;
// The program header precedes the forward DFA. Offsets are relative to the program.
pub const CAPTURE_PROGRAM: usize = 0;
/// Nonnegative offsets locate the reverse DFA. A negative value is the complement of the fixed match width in bytes.
pub const REVERSE_OFFSET: usize = 4;
pub const NAMES_OFFSET: usize = 8;
pub const GROUP_COUNT: usize = 12;
pub const PROGRAM_HEADER: usize = 16;
pub const DFA_CLASSES: usize = 8;
pub const DFA_ROWS: usize = DFA_CLASSES + 4;
pub const GLOBAL: usize = 14;
pub const NFA_HEADER: usize = 4;

codes!(NfaOp { Fail = 0, Ranges = 1, Look = 2, Union = 3, Capture = 4, Match = 5 });
codes!(LookCode {
    Start = 1, End = 2, StartLine = 3, EndLine = 4,
    WordBoundary = 5, NotWordBoundary = 6, WordStart = 7, WordEnd = 8,
});

pub enum NfaState<'a> {
    Fail,
    Ranges(&'a [[u8; 4]]),
    Look(LookCode, u16),
    Union(&'a [[u8; 2]]),
    Capture(u8, u16),
    Match,
}

pub struct NfaTable<'a> {
    offsets: &'a [[u8; 2]],
    body: &'a [u8],
    start: u16,
}

impl<'a> NfaTable<'a> {
    /// `bytes` is a complete NFA table, including its state offsets.
    pub fn read(bytes: &'a [u8]) -> Self {
        let count = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        let start = u16::from_le_bytes([bytes[2], bytes[3]]);
        assert!(
            count <= MAX_STATES && usize::from(start) < count,
            "regex NFA states"
        );
        let (offsets, _) = bytes[NFA_HEADER..NFA_HEADER + count * 2].as_chunks();
        Self {
            offsets,
            body: &bytes[NFA_HEADER + count * 2..],
            start,
        }
    }

    pub fn start(&self) -> u16 {
        self.start
    }

    pub fn state(&self, id: u16) -> NfaState<'a> {
        let id = usize::from(id);
        let offset = u16::from_le_bytes(self.offsets[id]) as usize;
        let end = self
            .offsets
            .get(id + 1)
            .map_or(self.body.len(), |&end| u16::from_le_bytes(end) as usize);
        let bytes = &self.body[offset..end];
        let args = &bytes[1..];
        match NfaOp::try_from(u32::from(bytes[0])).expect("invalid regex NfaOp") {
            NfaOp::Fail => NfaState::Fail,
            NfaOp::Ranges => NfaState::Ranges(args.as_chunks().0),
            NfaOp::Look => NfaState::Look(
                LookCode::try_from(u32::from(args[0])).expect("invalid regex LookCode"),
                u16::from_le_bytes([args[1], args[2]]),
            ),
            NfaOp::Union => NfaState::Union(args.as_chunks().0),
            NfaOp::Capture => NfaState::Capture(args[0], u16::from_le_bytes([args[1], args[2]])),
            NfaOp::Match => NfaState::Match,
        }
    }
}
