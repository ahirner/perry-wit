//! Raw-pointer access to runtime JSON nodes.
pub use perry_abi::json::*;
const TAIL: usize = 8;

/// Read a 32-bit word at `offset` bytes from `node`.
///
/// # Safety
/// `node.add(offset)` must be valid for an aligned 32-bit read.
pub unsafe fn word(node: *mut u8, offset: usize) -> u32 {
    unsafe { *(node.add(offset) as *const u32) }
}

/// Write a 32-bit word at `offset` bytes from `node`.
///
/// # Safety
/// `node.add(offset)` must be valid for an aligned 32-bit write.
pub unsafe fn set_word(node: *mut u8, offset: usize, value: u32) {
    unsafe { *(node.add(offset) as *mut u32) = value }
}

pub fn node(address: u32) -> *mut u8 {
    address as usize as *mut u8
}

pub fn address(node: *mut u8) -> u32 {
    node as usize as u32
}

/// A fresh zeroed node of `kind`.
pub fn new_node(kind: u32) -> *mut u8 {
    use self::kind::*;
    let shape: &'static [u32] = match kind {
        STRING => &[0, 3, 4, 8, 24],
        EXCEPTION | NATIVE | VIEW | BIGINT | ARRAY | OBJECT => &[0, 4, 4, 8, 16, 24],
        _ => &[0, 2, 4, 24],
    };
    let node = unsafe { crate::alloc_shaped(NODE_BYTES, shape) };
    unsafe {
        core::ptr::write_bytes(node, 0, NODE_BYTES as usize);
        set_word(node, 0, kind);
    }
    node
}

/// Look up a field in the compiler's table sorted by UTF-8 length, then bytes.
///
/// # Safety
/// `table` contains `count` pairs of live string addresses and field indices.
pub unsafe fn field_index(
    table: *const u32,
    count: u32,
    compare: impl Fn(&[u8]) -> core::cmp::Ordering,
) -> u32 {
    let names = unsafe { core::slice::from_raw_parts(table, count as usize * 2) }
        .as_chunks::<2>()
        .0;
    let compare = |entry: &[u32; 2]| compare(unsafe { crate::bytes(node(entry[0])) }).reverse();
    let index = if count < 16 {
        names.iter().position(|entry| compare(entry).is_eq())
    } else {
        names.binary_search_by(compare).ok()
    };
    index.map_or(count, |i| names[i][1])
}

/// Resolve a storage index in the compiler's indexed name table.
///
/// # Safety
/// `table` contains `count` pairs with each field index exactly once.
pub unsafe fn field_name(table: *const u32, count: u32, index: u32) -> *mut u8 {
    let names = unsafe { core::slice::from_raw_parts(table, count as usize * 2) }
        .as_chunks::<2>()
        .0;
    node(
        names
            .iter()
            .find(|entry| entry[1] == index)
            .expect("record field in schema")[0],
    )
}

/// Append an unlinked child to a JSON container under construction.
///
/// # Safety
/// Both pointers and the container's existing tail must address live, aligned `NODE_BYTES`
/// blocks in Wasm32 memory. The caller owns the container and child's link fields.
pub unsafe fn link_child(container: *mut u8, child: *mut u8) {
    unsafe {
        let last = word(container, TAIL);
        if last == 0 {
            set_word(container, FIRST, address(child));
        } else {
            set_word(node(last), NEXT, address(child));
        }
        set_word(container, TAIL, address(child));
        set_word(container, MEMBER_COUNT, word(container, MEMBER_COUNT) + 1);
    }
}
