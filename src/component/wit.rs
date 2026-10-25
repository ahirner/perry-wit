//! WIT package resolution shared with the standalone contract crate.
pub(crate) use perry_contract::resolution::{
    resolve_contents, resolve_embedded_source, select_world,
};
pub use perry_contract::resolution::{resolve_source, resolve_wit};
