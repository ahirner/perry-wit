//! Native byte transfers and managed Web stream objects.

pub(crate) mod buffered;
pub(crate) mod incoming;
pub(crate) mod output;
pub(crate) mod transfer;
pub(crate) mod web;
mod writable;

pub(crate) use transfer::read as emit_read_transfer;

#[cfg(test)]
#[path = "streams/buffered_test.rs"]
mod buffered_test;
