//! Fetch iterates a sorted, combined projection of the insertion-ordered header list.
use super::*;

#[repr(C)]
struct Cursor {
    owner: *mut u8,
    order: Buffer,
    key: *mut u8,
    value: *mut u8,
    revision: u64,
    position: usize,
    raw: usize,
    projection: u32,
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_headers_iterator(owner: *mut u8, projection: u32) -> *mut u8 {
    let cursor = perry_rt_sys::object!(Cursor; owner, order, key, value);
    unsafe {
        cursor.write(Cursor {
            owner,
            order: Buffer::new(),
            key: core::ptr::null_mut(),
            value: core::ptr::null_mut(),
            revision: u64::MAX,
            position: 0,
            raw: 0,
            projection,
        })
    };
    cursor.cast()
}

fn field(fields: &[[u8; 16]], index: &[u8; 4]) -> (&'static [u8], &'static [u8]) {
    field_parts(&fields[u32::from_le_bytes(*index) as usize])
}

fn group_length(fields: &[[u8; 16]], order: &[[u8; 4]]) -> usize {
    let name = field(fields, &order[0]).0;
    if name == b"set-cookie" {
        1
    } else {
        order.partition_point(|i| field(fields, i).0 == name)
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_headers_advance(cursor: *mut u8) -> u32 {
    let cursor = unsafe { &mut *cursor.cast::<Cursor>() };
    let owner = headers(cursor.owner);
    let fields = owner.fields.as_slice().as_chunks::<16>().0;
    if cursor.revision != owner.revision {
        cursor.order.clear();
        for index in 0..fields.len() as u32 {
            cursor.order.extend_from_slice(&index.to_le_bytes());
        }
        let order = cursor.order.as_mut_slice().as_chunks_mut::<4>().0;
        perry_rt_sys::heap_sort(order, |a, b| {
            field(fields, a)
                .0
                .cmp(field(fields, b).0)
                .then_with(|| u32::from_le_bytes(*a).cmp(&u32::from_le_bytes(*b)))
        });
        cursor.raw = 0;
        for _ in 0..cursor.position {
            if cursor.raw == order.len() {
                break;
            }
            cursor.raw += group_length(fields, &order[cursor.raw..]);
        }
        cursor.revision = owner.revision;
    }
    let remaining = &cursor.order.as_slice().as_chunks::<4>().0[cursor.raw..];
    if remaining.is_empty() {
        return 0;
    }
    let len = group_length(fields, remaining);
    if cursor.projection != 2 {
        let name = field(fields, &remaining[0]).0;
        // Normalized field names are immutable ASCII owned by the traced header list.
        cursor.key = unsafe {
            perry_rt_sys::adopt_counted(
                name.as_ptr().cast_mut(),
                name.len() as u32,
                name.len() as u32,
            )
        };
    }
    if cursor.projection != 1 {
        cursor.value = values_string(remaining[..len].iter().map(|i| field(fields, i).1));
    }
    cursor.raw += len;
    cursor.position += 1;
    1
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_headers_key(cursor: *mut u8) -> *mut u8 {
    unsafe { (*cursor.cast::<Cursor>()).key }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_headers_value(cursor: *mut u8) -> *mut u8 {
    let cursor = unsafe { &*cursor.cast::<Cursor>() };
    if cursor.projection == 1 {
        cursor.key
    } else {
        cursor.value
    }
}
