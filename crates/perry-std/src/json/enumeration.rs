//! Apply instance property order to a canonical native-field projection.
use super::order::std_json_record_index;
use perry_abi::{
    json::{FIRST, NEXT, PACKED_FIELDS, array_index, node, set_word, word},
    layout::{array, record},
};

unsafe fn visit_fields(
    native: *mut u8,
    table: *const u32,
    count: u32,
    retained: bool,
    mut visit: impl FnMut(usize),
) {
    unsafe {
        let graph = if retained {
            word(native, record::JSON as usize)
        } else {
            0
        };
        if graph != 0 {
            let mut member = word(node(graph), FIRST);
            while member != 0 {
                let index = std_json_record_index(member, table, count);
                if index < count {
                    visit(index as usize);
                }
                member = word(node(member), NEXT);
            }
        } else {
            let mut order = word(native, record::ORDER as usize);
            if count as usize <= PACKED_FIELDS {
                while order != 0 {
                    visit(((order & 15) - 1) as usize);
                    order >>= 4;
                }
            } else if order != 0 {
                for position in 0..word(node(order), 0) {
                    visit(word(node(order), 4 * (position as usize + 1)) as usize);
                }
            }
        }
    }
}

/// `projection` contains every native field in canonical layout order.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_record_order(
    native: *mut u8,
    projection: *mut u8,
    table: *const u32,
    count: u32,
    stride: u32,
    retained: u32,
) {
    unsafe {
        const ABSENT: u32 = u32::MAX;
        const UNRANKED: u32 = u32::MAX - 1;
        let mut inline = [ABSENT; 64];
        let destinations = if count <= 64 {
            &mut inline[..count as usize]
        } else {
            core::slice::from_raw_parts_mut(
                perry_abi::alloc(count * 4).cast::<u32>(),
                count as usize,
            )
        };
        destinations.fill(ABSENT);
        visit_fields(native, table, count, retained != 0, |index| {
            destinations[index] = UNRANKED;
        });
        let mut next = 0;
        // The shared name table sorts by byte length, then bytes: numeric keys are ascending.
        for entry in 0..count as usize {
            let name = *table.add(entry * 2) as *mut u8;
            let index = *table.add(entry * 2 + 1) as usize;
            if destinations[index] == UNRANKED && array_index(crate::runtime::bytes(name)).is_some()
            {
                destinations[index] = next;
                next += 1;
            }
        }
        visit_fields(native, table, count, retained != 0, |index| {
            if destinations[index] == UNRANKED {
                destinations[index] = next;
                next += 1;
            }
        });
        let length = next as usize;
        // Absent fields occupy the unused tail, completing a permutation of all slots.
        for destination in destinations.iter_mut() {
            if *destination == ABSENT {
                *destination = next;
                next += 1;
            }
        }
        let data = node(word(projection, array::DATA as usize));
        for index in 0..length {
            while destinations[index] as usize != index {
                let destination = destinations[index] as usize;
                core::ptr::swap_nonoverlapping(
                    data.add(index * stride as usize),
                    data.add(destination * stride as usize),
                    stride as usize,
                );
                destinations.swap(index, destination);
            }
        }
        set_word(projection, array::LEN as usize, length as u32);
    }
}
