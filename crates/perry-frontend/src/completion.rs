//! Return completion facts shared by inference and elaboration.

pub(crate) const FALLTHROUGH: u8 = 1;
pub(crate) const RETURN: u8 = 2;
pub(crate) const BREAK: u8 = 4;
pub(crate) const CONTINUE: u8 = 8;
pub(crate) const LABELLED: u8 = 16;
// Return-type inference also sees syntactically present returns on unreachable paths.
pub(crate) const EMPTY_RETURN: u8 = 32;

pub(crate) fn sequence(first: u8, second: u8) -> u8 {
    (first & !FALLTHROUGH)
        | if first & FALLTHROUGH != 0 {
            second
        } else {
            second & EMPTY_RETURN
        }
}

pub(crate) fn loop_exits(body: u8, infinite: bool) -> u8 {
    (body & (RETURN | EMPTY_RETURN | LABELLED))
        | if infinite && body & BREAK == 0 {
            0
        } else {
            FALLTHROUGH
        }
}
