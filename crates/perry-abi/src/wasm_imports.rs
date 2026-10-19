//! Shared declarations for linked imports and native substitutes.

/// Declare each external signature once for Wasm and native builds.
#[macro_export]
macro_rules! wasm_imports {
    ($module:literal => $host:path; $(
        fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $result:ty)?;
    )*) => {
        #[cfg(target_arch = "wasm32")]
        #[link(wasm_import_module = $module)]
        unsafe extern "C" {
            $(pub(super) fn $name($($arg: $ty),*) $(-> $result)?;)*
        }
        #[cfg(not(target_arch = "wasm32"))]
        pub(super) use $host::{$($name),*};
    };
    ($module:literal; $(
        $(#[$attr:meta])* $vis:vis fn $name:ident($($arg:ident: $ty:ty),* $(,)?)
        $(-> $result:ty)? $(=> $native:block)?;
    )*) => {
        #[cfg(target_arch = "wasm32")]
        #[link(wasm_import_module = $module)]
        unsafe extern "C" {
            $($(#[$attr])* $vis fn $name($($arg: $ty),*) $(-> $result)?;)*
        }
        $(
            #[cfg(not(target_arch = "wasm32"))]
            $crate::wasm_imports!(@native $module; $vis fn $name($($arg: $ty),*)
                $(-> $result)? $(=> $native)?);
        )*
    };
    (@native $module:literal; $vis:vis fn $name:ident($($arg:ident: $ty:ty),*)
        $(-> $result:ty)? => $native:block) => {
        /// # Safety
        /// The caller must satisfy the imported function's buffer and handle contract.
        $vis unsafe fn $name($($arg: $ty),*) $(-> $result)? $native
    };
    (@native $module:literal; $vis:vis fn $name:ident($($arg:ident: $ty:ty),*)
        $(-> $result:ty)?) => {
        /// # Safety
        /// This import is available only in linked Wasm modules.
        $vis unsafe fn $name($(_: $ty),*) $(-> $result)? {
            unreachable!(concat!($module, "::", stringify!($name), " requires a linked Wasm module"))
        }
    };
}
