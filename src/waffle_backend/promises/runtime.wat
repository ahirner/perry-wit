(core module $promise-runtime
  (import "guest" "memory" (memory 1))
  (import "guest" "cabi_realloc" (func $alloc (param i32 i32 i32 i32) (result i32)))
  (import "native" "new-set" (func $new-set (result i32)))
  (import "native" "join" (func $join (param i32 i32)))
  (import "native" "wait" (func $wait (param i32 i32) (result i32)))
  (import "native" "drop-task" (func $drop-task (param i32)))
  (import "native" "drop-set" (func $drop-set (param i32)))
  (import "native" "yield" (func $yield (result i32)))
  (import "native" "thread-index" (func $thread-index (result i32)))
  (import "native" "suspend" (func $suspend (result i32)))
  (import "native" "resume" (func $resume (param i32)))

  (func (export "enter")
    (if (i32.load (i32.const 32)) (then unreachable))
    (i32.store (i32.const 32) (i32.const 1)))
  (func (export "leave") (i32.store (i32.const 32) (i32.const 0)))

  ;; One invocation owns the intrusive list rooted at memory[4]. Each 32-byte
  ;; record holds native status, language tag/payload, next pointer, and event space.
  ;; Offset 20 is 0 before observation, 1 for the sole native reader, or the head
  ;; of its FIFO of suspended observers. Allocated pointers cannot equal 0 or 1.
  (func (export "new") (result i32) (local $record i32)
    (local.set $record (call $alloc (i32.const 0) (i32.const 0) (i32.const 8) (i32.const 32)))
    (memory.fill (local.get $record) (i32.const 0) (i32.const 32))
    (i32.store offset=4 (local.get $record) (i32.const 2))
    (i32.store offset=16 (local.get $record) (i32.load (i32.const 4)))
    (i32.store (i32.const 4) (local.get $record))
    (local.get $record))

  (func (export "bind") (param $record i32) (param $status i32)
    (i32.store (local.get $record) (local.get $status)))

  (func (export "settle") (param $record i32) (param $tag i32) (param $payload f64)
    (if (i32.ne (i32.load offset=4 (local.get $record)) (i32.const 2)) (then unreachable))
    (if (i32.gt_u (local.get $tag) (i32.const 1)) (then unreachable))
    (f64.store offset=8 (local.get $record) (local.get $payload))
    (i32.store offset=4 (local.get $record) (local.get $tag)))

  (func (export "yield") (drop (call $yield)))

  (func (export "await") (param $record i32) (result i32 f64)
    (local $status i32) (local $task i32) (local $set i32) (local $event i32)
    (local $head i32) (local $waiter i32) (local $tail i32)
    (local.set $status (i32.load (local.get $record)))
    (if (i32.ne (i32.and (local.get $status) (i32.const 15)) (i32.const 2))
      (then
        (local.set $head (i32.load offset=20 (local.get $record)))
        (if (local.get $head)
          (then
            (local.set $waiter (call $alloc (i32.const 0) (i32.const 0) (i32.const 4) (i32.const 8)))
            (i32.store (local.get $waiter) (call $thread-index))
            (i32.store offset=4 (local.get $waiter) (i32.const 0))
            (if (i32.eq (local.get $head) (i32.const 1))
              (then (i32.store offset=20 (local.get $record) (local.get $waiter)))
              (else
                (local.set $tail (local.get $head))
                (block $end (loop $next
                  (br_if $end (i32.eqz (i32.load offset=4 (local.get $tail))))
                  (local.set $tail (i32.load offset=4 (local.get $tail)))
                  (br $next)))
                (i32.store offset=4 (local.get $tail) (local.get $waiter))))
            (drop (call $suspend)))
          (else
            (i32.store offset=20 (local.get $record) (i32.const 1))
            (local.set $task (i32.shr_u (local.get $status) (i32.const 4)))
            (local.set $set (call $new-set))
            (local.set $event (i32.add (local.get $record) (i32.const 24)))
            (call $join (local.get $task) (local.get $set))
            (loop $pending
              (if (i32.ne (call $wait (local.get $set) (local.get $event)) (i32.const 1)) (then unreachable))
              (if (i32.ne (i32.load (local.get $event)) (local.get $task)) (then unreachable))
              (br_if $pending (i32.ne (i32.load offset=4 (local.get $event)) (i32.const 2))))
            (call $join (local.get $task) (i32.const 0))
            (call $drop-task (local.get $task))
            (call $drop-set (local.get $set))
            (i32.store (local.get $record) (i32.const 2))
            (local.set $head (i32.load offset=20 (local.get $record)))
            (i32.store offset=20 (local.get $record) (i32.const 0))
            (block $done (loop $notify
              (br_if $done (i32.le_u (local.get $head) (i32.const 1)))
              (call $resume (i32.load (local.get $head)))
              (local.set $head (i32.load offset=4 (local.get $head)))
              (br $notify)))))))
    (drop (call $yield))
    (if (i32.gt_u (i32.load offset=4 (local.get $record)) (i32.const 1)) (then unreachable))
    (i32.load offset=4 (local.get $record))
    (f64.load offset=8 (local.get $record)))

  ;; Pending tasks cannot escape the serial invocation arena. Unsupported detached
  ;; work traps before result delivery; the host must discard that instance.
  (func (export "finish") (local $record i32)
    (local.set $record (i32.load (i32.const 4)))
    (block $done (loop $next
      (br_if $done (i32.eqz (local.get $record)))
      (if (i32.ne (i32.load (local.get $record)) (i32.const 2)) (then unreachable))
      (local.set $record (i32.load offset=16 (local.get $record)))
      (br $next)))
    (i32.store (i32.const 4) (i32.const 0))))
