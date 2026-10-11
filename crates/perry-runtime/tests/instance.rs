use perry_runtime::{
    RuntimeInstance,
    host::{BLOCKED, mock, output},
    region::{
        perry_alloc, perry_task_current, perry_task_enter, perry_task_free, perry_task_suspend,
    },
    roots::{perry_gc_counter, perry_gc_debug, perry_roots_state},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Barrier},
    thread,
};

#[test]
fn instances_keep_tasks_collectors_and_hosts_when_alternating_on_one_thread() {
    let mut first = RuntimeInstance::default();
    let mut second = RuntimeInstance::default();
    let first_roots = first.enter(|| perry_roots_state());
    let bytes = b"hello";
    let first_task = first.enter(|| {
        let task = perry_task_enter();
        perry_gc_debug(3);
        perry_alloc(32);
        assert_eq!(output::write(0, 0, bytes.as_ptr(), 5), 5 << 4);
        assert_eq!(output::write(0, 0, bytes.as_ptr(), 5), BLOCKED);
        task
    });
    let second_task = second.enter(|| {
        assert!(perry_task_current().is_null());
        assert_ne!(perry_roots_state(), first_roots);
        assert_eq!(perry_gc_counter(0), 0);
        assert_eq!(mock::complete_pending(), None);
        assert_eq!(output::write(0, 0, bytes.as_ptr(), 5), 5 << 4);
        perry_task_enter()
    });
    assert_ne!(first_task, second_task);
    first.enter(|| {
        assert_eq!(perry_roots_state(), first_roots);
        assert_eq!(perry_task_current(), first_task);
        assert_eq!(mock::complete_pending(), Some((11, 5 << 4)));
        perry_task_free();
    });
    second.enter(|| {
        assert_eq!(perry_task_current(), second_task);
        assert_eq!(mock::complete_pending(), None);
        perry_task_free();
    });
}

#[test]
fn nested_activation_restores_the_owner_after_unwinding() {
    let mut outer = RuntimeInstance::default();
    let mut inner = RuntimeInstance::default();
    outer.enter(|| {
        let task = perry_task_enter();
        let result = catch_unwind(AssertUnwindSafe(|| {
            inner.enter(|| {
                assert!(perry_task_current().is_null());
                perry_task_enter();
                perry_alloc(100_000);
                perry_task_suspend();
                panic!("leave a suspended task owned by the inner instance");
            });
        }));
        assert!(result.is_err());
        drop(inner);
        assert_eq!(perry_task_current(), task);
        perry_task_free();
    });
}

#[test]
fn concurrent_instances_keep_their_active_tasks() {
    let barrier = Arc::new(Barrier::new(4));
    thread::scope(|scope| {
        for _ in 0..4 {
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                RuntimeInstance::default().enter(|| {
                    let task = perry_task_enter();
                    perry_alloc(32);
                    barrier.wait();
                    assert_eq!(perry_task_current(), task);
                    perry_task_free();
                });
            });
        }
    });
}
