//! P2 filesystem bindings for the existing node:fs promise implementation.
#![no_std]
use core::{mem::ManuallyDrop, ptr};
use perry_p2_sys::{self as sys, DROPPED, Operation, wasip2};
use wasip2::filesystem::types::*;

unsafe fn descriptor(handle: u32) -> ManuallyDrop<Descriptor> {
    ManuallyDrop::new(unsafe { Descriptor::from_handle(handle) })
}
fn kind(value: DescriptorType) -> u8 {
    match value {
        DescriptorType::Unknown => 7,
        other => other as u8 - 1,
    }
}
fn error_case(error: ErrorCode) -> u8 {
    // P3 removed `would-block` from the enum and added `other` at its end.
    match error {
        ErrorCode::Access => 0,
        ErrorCode::WouldBlock => 12, // I/O error; these descriptor operations cannot await it.
        other => other as u8 - 1,
    }
}
unsafe fn result_error(area: *mut u8, offset: usize, error: ErrorCode) {
    unsafe {
        *area = 1;
        *area.add(offset) = error_case(error);
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_open(params: *const u8, result: *mut u8) -> u32 {
    unsafe {
        ptr::write_bytes(result, 0, 24);
    }
    let word = |offset| unsafe { params.add(offset).cast::<u32>().read() };
    let value = unsafe { descriptor(word(0)) }.open_at(
        PathFlags::from_bits_retain(unsafe { *params.add(4) }),
        unsafe { sys::text(word(8) as *const u8, word(12)) },
        OpenFlags::from_bits_retain(unsafe { *params.add(16) }),
        DescriptorFlags::from_bits_retain(unsafe { *params.add(17) }),
    );
    match value {
        Ok(fd) => unsafe { result.add(4).cast::<u32>().write(fd.take_handle()) },
        Err(error) => unsafe { result_error(result, 4, error) },
    }
    2
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_stat(
    fd: u32,
    flags: u32,
    path: *const u8,
    len: u32,
    result: *mut u8,
) -> u32 {
    unsafe {
        ptr::write_bytes(result, 0, 112);
    }
    match unsafe { descriptor(fd) }.stat_at(PathFlags::from_bits_retain(flags as u8), unsafe {
        sys::text(path, len)
    }) {
        Ok(stat) => unsafe {
            *result.add(8) = kind(stat.type_);
            result.add(24).cast::<u64>().write(stat.link_count);
            result.add(32).cast::<u64>().write(stat.size);
            for (offset, timestamp) in [
                (40, stat.data_access_timestamp),
                (64, stat.data_modification_timestamp),
                (88, stat.status_change_timestamp),
            ] {
                if let Some(value) = timestamp {
                    *result.add(offset) = 1;
                    result.add(offset + 8).cast::<u64>().write(value.seconds);
                    result
                        .add(offset + 16)
                        .cast::<u32>()
                        .write(value.nanoseconds);
                }
            }
        },
        Err(error) => unsafe { result_error(result, 8, error) },
    }
    2
}
macro_rules! path_operation {
    ($name:ident, $method:ident) => {
        #[unsafe(no_mangle)]
        pub(crate) unsafe extern "C" fn $name(
            fd: u32,
            path: *const u8,
            len: u32,
            result: *mut u8,
        ) -> u32 {
            unsafe {
                ptr::write_bytes(result, 0, 24);
            }
            if let Err(error) = unsafe { descriptor(fd) }.$method(unsafe { sys::text(path, len) }) {
                unsafe { result_error(result, 4, error) };
            }
            2
        }
    };
}
path_operation!(p2_fs_mkdir, create_directory_at);
path_operation!(p2_fs_rmdir, remove_directory_at);
path_operation!(p2_fs_unlink, unlink_file_at);

struct Transfer {
    fd: u32,
    offset: u64,
    completion: u32,
    directory: Option<DirectoryEntryStream>,
}
fn completion() -> u32 {
    let id = sys::completion(24, false);
    unsafe {
        sys::op(id).error_offset = 4;
        sys::op(id).refs += 1;
    }
    id
}
fn transfer(fd: u32, offset: u64, completion: u32, directory: Option<DirectoryEntryStream>) -> u32 {
    Operation::new(0, 0)
        .with(
            Transfer {
                fd,
                offset,
                completion,
                directory,
            },
            None,
        )
        .register()
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_read_stream(fd: u32, offset: u64, result: *mut u32) {
    let future = completion();
    let id = transfer(fd, offset, future, None);
    unsafe {
        *result = id;
        *result.add(1) = future;
    }
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_fs_write_new() -> u64 {
    let id = transfer(0, 0, 0, None);
    u64::from(id) | (u64::from(id) << 32)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_write_stream(fd: u32, stream: u32, offset: u64) -> u32 {
    let transfer = unsafe { sys::payload::<Transfer>(sys::op(stream)) };
    transfer.fd = fd;
    transfer.offset = offset;
    transfer.completion = completion();
    transfer.completion
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_read(stream: u32, buffer: *mut u8, len: u32) -> u32 {
    let transfer = unsafe { sys::payload::<Transfer>(sys::op(stream)) };
    match unsafe { descriptor(transfer.fd) }.read(u64::from(len), transfer.offset) {
        Ok((bytes, eof)) => {
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
            }
            transfer.offset += bytes.len() as u64;
            ((bytes.len() as u32) << 4) | u32::from(eof)
        }
        Err(error) => {
            unsafe {
                sys::op(transfer.completion).error = Some(error_case(error));
            }
            DROPPED
        }
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_write(stream: u32, data: *const u8, len: u32) -> u32 {
    let transfer = unsafe { sys::payload::<Transfer>(sys::op(stream)) };
    match unsafe { descriptor(transfer.fd) }
        .write(unsafe { sys::bytes(data, len) }, transfer.offset)
    {
        Ok(count) => {
            transfer.offset += count;
            (count as u32) << 4
        }
        Err(error) => {
            unsafe {
                sys::op(transfer.completion).error = Some(error_case(error));
            }
            DROPPED
        }
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_drop_stream(stream: u32) {
    let transfer = unsafe { sys::payload::<Transfer>(sys::op(stream)) };
    if transfer.completion != 0 {
        unsafe {
            sys::finish(sys::op(transfer.completion));
            sys::p2_drop(transfer.completion);
        }
    }
    unsafe { sys::p2_drop(stream) };
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_directory(fd: u32, result: *mut u32) {
    let future = completion();
    let directory = match unsafe { descriptor(fd) }.read_directory() {
        Ok(directory) => Some(directory),
        Err(error) => {
            unsafe {
                sys::op(future).error = Some(error_case(error));
            }
            None
        }
    };
    let stream = transfer(fd, 0, future, directory);
    unsafe {
        *result = stream;
        *result.add(1) = future;
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_fs_entries(stream: u32, buffer: *mut u8, count: u32) -> u32 {
    let transfer = unsafe { sys::payload::<Transfer>(sys::op(stream)) };
    let Some(directory) = transfer.directory.as_ref() else {
        return DROPPED;
    };
    for index in 0..count {
        match directory.read_directory_entry() {
            Ok(Some(entry)) => unsafe {
                let at = buffer.add(index as usize * 24);
                ptr::write_bytes(at, 0, 24);
                *at = kind(entry.type_);
                let name = sys::cabi_realloc(ptr::null_mut(), 0, 1, entry.name.len() as u32);
                ptr::copy_nonoverlapping(entry.name.as_ptr(), name, entry.name.len());
                at.add(16).cast::<u32>().write(name as u32);
                at.add(20).cast::<u32>().write(entry.name.len() as u32);
            },
            Ok(None) => return (index << 4) | DROPPED,
            Err(error) => {
                unsafe {
                    sys::op(transfer.completion).error = Some(error_case(error));
                }
                return (index << 4) | DROPPED;
            }
        }
    }
    count << 4
}
