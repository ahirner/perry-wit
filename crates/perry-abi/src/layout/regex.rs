//! Compiler-emitted regex tables. Invalid tables indicate a compiler bug and trap.
pub const MAX_CAPTURES: usize = 8;
pub const MAX_STATES: usize = 256;
pub const MAX_THREADS: usize = 24;
pub const MAX_PENDING: usize = 64;
pub const REVERSE_CLASSES: usize = 8;
pub const FORWARD_CLASSES: usize = 12;
pub const REVERSE_ROWS: usize = REVERSE_CLASSES + 256;
pub const FORWARD_ROWS: usize = FORWARD_CLASSES + 256;
pub const GLOBAL: u32 = 1 << 31;
pub const NFA_HEADER: usize = 8;

macro_rules! codes {
    ($name:ident { $($variant:ident = $code:literal),+ $(,)? }) => {
        #[derive(Clone, Copy)]
        #[repr(u8)]
        pub enum $name { $($variant = $code),+ }

        impl $name {
            pub fn decode(code: u8) -> Self {
                match code {
                    $($code => Self::$variant,)+
                    _ => panic!(concat!("invalid regex ", stringify!($name))),
                }
            }
        }
    };
}

codes!(NfaOp { Fail = 0, Range = 1, Sparse = 2, Look = 3, Union = 4, Capture = 5, Match = 6 });
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
    /// `bytes` is a complete NFA table, including its byte length and state offsets.
    pub fn read(bytes: &'a [u8]) -> Self {
        let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        let start = u16::from_le_bytes([bytes[6], bytes[7]]);
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
        match NfaOp::decode(bytes[0]) {
            NfaOp::Fail => NfaState::Fail,
            NfaOp::Range => NfaState::Ranges(args[..4].as_chunks().0),
            NfaOp::Sparse => {
                let count = u16::from_le_bytes([args[0], args[1]]) as usize;
                NfaState::Ranges(args[2..2 + count * 4].as_chunks().0)
            }
            NfaOp::Look => NfaState::Look(
                LookCode::decode(args[0]),
                u16::from_le_bytes([args[1], args[2]]),
            ),
            NfaOp::Union => {
                let count = u16::from_le_bytes([args[0], args[1]]) as usize;
                NfaState::Union(args[2..2 + count * 2].as_chunks().0)
            }
            NfaOp::Capture => NfaState::Capture(args[0], u16::from_le_bytes([args[1], args[2]])),
            NfaOp::Match => NfaState::Match,
        }
    }
}
