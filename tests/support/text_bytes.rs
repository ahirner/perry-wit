//! Host representation shared by native string/byte component boundary tests.
use wasmtime::component::{ComponentType, Lift, Lower};

#[derive(ComponentType, Lift, Lower, Debug, Clone, PartialEq)]
#[component(variant)]
pub enum TextOrBytes {
    #[component(name = "text")]
    Text(String),
    #[component(name = "bytes")]
    Bytes(Vec<u8>),
}
