/// Select the guest's static storage or a field of the explicitly active native instance.
macro_rules! instance_state {
    ($name:ident, $access:ident, $field:ident, $state:ty, $initial:expr) => {
        #[cfg(target_arch = "wasm32")]
        static $name: Global = Global(UnsafeCell::new($initial));

        fn $access() -> *mut $state {
            #[cfg(target_arch = "wasm32")]
            {
                $name.0.get()
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                $crate::instance::native::with(|instance| instance.$field.get())
            }
        }
    };
}

pub(crate) use instance_state;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native {
    use crate::{
        host::mock::Host,
        io::Process,
        region::{NativeStorage, Runtime},
        resources::Registry,
        roots::Roots,
    };
    use std::{
        cell::{RefCell, UnsafeCell},
        rc::Rc,
    };

    pub(crate) struct State {
        pub(crate) runtime: UnsafeCell<Runtime>,
        pub(crate) roots: UnsafeCell<Option<Box<Roots>>>,
        pub(crate) registry: UnsafeCell<Registry>,
        pub(crate) process: UnsafeCell<Process>,
        pub(crate) storage: RefCell<NativeStorage>,
        pub(crate) host: RefCell<Host>,
    }

    std::thread_local! {
        static ACTIVE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
    }

    /// Owns one native runtime instance, including allocations and its mock host.
    /// Guest ABI calls run within `enter`, and separate instances can alternate on a single thread.
    pub struct RuntimeInstance {
        state: Rc<State>,
    }

    impl Default for RuntimeInstance {
        fn default() -> Self {
            Self {
                state: Rc::new(State {
                    runtime: UnsafeCell::new(Runtime::new()),
                    roots: UnsafeCell::new(None),
                    registry: UnsafeCell::new(Registry::new()),
                    process: UnsafeCell::new(Process::new()),
                    storage: RefCell::new(NativeStorage::default()),
                    host: RefCell::new(Host::default()),
                }),
            }
        }
    }

    impl RuntimeInstance {
        /// Activate this instance for synchronous ABI calls, restoring the previous one on unwind.
        /// Returned guest pointers remain valid only while their owning instance and allocations live.
        /// Pointer-based ABI calls must run with the instance that owns those pointers active.
        pub fn enter<R>(&mut self, run: impl FnOnce() -> R) -> R {
            let _activation = Activation(ACTIVE.replace(Some(Rc::clone(&self.state))));
            run()
        }
    }

    struct Activation(Option<Rc<State>>);

    impl Drop for Activation {
        fn drop(&mut self) {
            ACTIVE.set(self.0.take());
        }
    }

    pub(crate) fn with<R>(access: impl FnOnce(&State) -> R) -> R {
        let state = ACTIVE.with_borrow(|active| {
            Rc::clone(
                active
                    .as_ref()
                    .expect("native runtime requires RuntimeInstance::enter"),
            )
        });
        access(&state)
    }
}
