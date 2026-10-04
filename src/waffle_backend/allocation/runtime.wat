(module
  (import "heap" "bump" (func $bump (param i32 i32 i32 i32) (result i32)))
  (import "heap" "memory" (memory 1))
  ;; Memory[36,40,44] holds block head, block tail, and active root-frame head.
  ;; Block headers: next, span, payload offset, payload size, kind, mark, reserved.
  ;; The word immediately before every payload points back to its block header.
  ;; Kinds: 0 bytes, 1 string, 2 string array, 3 scalar Promise, 4 reference Promise,
  ;; 5 observer, 6 root frame, 7 byte view, 8 decoder, 9 object, 10 property, 11 boxed value;
  ;; -1 free. Marks: 0 white, 1 gray, 2 black.

  (func $aligned (param $value i32) (param $alignment i32) (result i32)
    (local $result i64)
    (local.set $result (i64.and
      (i64.add (i64.extend_i32_u (local.get $value)) (i64.sub (i64.extend_i32_u (local.get $alignment)) (i64.const 1)))
      (i64.sub (i64.const 0) (i64.extend_i32_u (local.get $alignment)))))
    (if (i64.gt_u (local.get $result) (i64.const 4294967295)) (then unreachable))
    (i32.wrap_i64 (local.get $result)))

  (func $find (param $pointer i32) (result i32)
    (local $block i32) (local $start i32)
    (local.set $block (i32.load (i32.const 36)))
    (block $done (loop $next
      (br_if $done (i32.eqz (local.get $block)))
      (if (i32.ne (i32.load offset=16 (local.get $block)) (i32.const -1)) (then
        (local.set $start (i32.add (local.get $block) (i32.load offset=8 (local.get $block))))
        (if (i32.and (i32.ge_u (local.get $pointer) (local.get $start))
          (i32.lt_u (local.get $pointer) (i32.add (local.get $start) (i32.load offset=12 (local.get $block)))))
          (then (return (local.get $block))))))
      (local.set $block (i32.load (local.get $block)))
      (br $next)))
    (i32.const 0))

  (func $allocate (param $alignment i32) (param $size i32) (result i32)
    (local $block i32) (local $pointer i32) (local $tail i32) (local $end i64) (local $total i64)
    (if (i32.or (i32.eqz (local.get $alignment))
      (i32.ne (i32.and (local.get $alignment) (i32.sub (local.get $alignment) (i32.const 1))) (i32.const 0))) (then unreachable))
    (if (i32.eqz (local.get $size)) (then (return (i32.const 0))))
    (if (i32.eqz (i32.load (i32.const 0))) (then
      (i32.store (i32.const 36) (i32.const 0))
      (i32.store (i32.const 40) (i32.const 0))
      (i32.store (i32.const 44) (i32.const 0))))
    (local.set $block (i32.load (i32.const 36)))
    (block $found (loop $next
      (br_if $found (i32.eqz (local.get $block)))
      (if (i32.eq (i32.load offset=16 (local.get $block)) (i32.const -1)) (then
        (local.set $pointer (call $aligned (i32.add (local.get $block) (i32.const 32)) (local.get $alignment)))
        (br_if $found (i64.le_u
          (i64.add (i64.extend_i32_u (local.get $pointer)) (i64.extend_i32_u (local.get $size)))
          (i64.add (i64.extend_i32_u (local.get $block)) (i64.extend_i32_u (i32.load offset=4 (local.get $block))))))))
      (local.set $block (i32.load (local.get $block)))
      (br $next)))
    (if (i32.eqz (local.get $block)) (then
      (local.set $total (i64.add (i64.add (i64.extend_i32_u (local.get $size)) (i64.const 31)) (i64.extend_i32_u (local.get $alignment))))
      (if (i64.gt_u (local.get $total) (i64.const 4294967295)) (then unreachable))
      (local.set $block (call $bump (i32.const 0) (i32.const 0) (i32.const 16) (i32.wrap_i64 (local.get $total))))
      (i32.store (local.get $block) (i32.const 0))
      (i32.store offset=4 (local.get $block) (i32.wrap_i64 (local.get $total)))
      (local.set $tail (i32.load (i32.const 40)))
      (if (local.get $tail)
        (then (i32.store (local.get $tail) (local.get $block)))
        (else (i32.store (i32.const 36) (local.get $block))))
      (i32.store (i32.const 40) (local.get $block))
      (local.set $pointer (call $aligned (i32.add (local.get $block) (i32.const 32)) (local.get $alignment)))))
    (local.set $end (i64.add (i64.extend_i32_u (local.get $block)) (i64.extend_i32_u (i32.load offset=4 (local.get $block)))))
    (local.set $total (i64.and (i64.add (i64.add (i64.extend_i32_u (local.get $pointer)) (i64.extend_i32_u (local.get $size))) (i64.const 15)) (i64.const -16)))
    (if (i64.le_u (i64.add (local.get $total) (i64.const 48)) (local.get $end)) (then
      (local.set $tail (i32.wrap_i64 (local.get $total)))
      (i32.store (local.get $tail) (i32.load (local.get $block)))
      (i32.store offset=4 (local.get $tail) (i32.wrap_i64 (i64.sub (local.get $end) (local.get $total))))
      (i32.store offset=16 (local.get $tail) (i32.const -1))
      (i32.store offset=20 (local.get $tail) (i32.const 0))
      (i32.store (local.get $block) (local.get $tail))
      (i32.store offset=4 (local.get $block) (i32.sub (local.get $tail) (local.get $block)))
      (if (i32.eq (i32.load (i32.const 40)) (local.get $block))
        (then (i32.store (i32.const 40) (local.get $tail))))))
    (i32.store offset=8 (local.get $block) (i32.sub (local.get $pointer) (local.get $block)))
    (i32.store offset=12 (local.get $block) (local.get $size))
    (i32.store offset=16 (local.get $block) (i32.const 0))
    (i32.store offset=20 (local.get $block) (i32.const 0))
    (i32.store (i32.sub (local.get $pointer) (i32.const 4)) (local.get $block))
    (local.get $pointer))

  (func $realloc (export "cabi_realloc") (param $old i32) (param $old_size i32) (param $alignment i32) (param $size i32) (result i32)
    (local $pointer i32) (local $block i32)
    (if (local.get $old) (then
      (local.set $block (call $find (local.get $old)))
      (if (i32.eqz (local.get $block)) (then unreachable))
      (if (i32.ne (local.get $old) (i32.add (local.get $block) (i32.load offset=8 (local.get $block)))) (then unreachable))
      (if (i32.gt_u (local.get $old_size) (i32.load offset=12 (local.get $block))) (then unreachable))))
    (local.set $pointer (call $allocate (local.get $alignment) (local.get $size)))
    (if (local.get $block) (then
      (memory.copy (local.get $pointer) (local.get $old) (select (local.get $size) (local.get $old_size) (i32.lt_u (local.get $size) (local.get $old_size))))
      (i32.store offset=16 (local.get $block) (i32.const -1))))
    (local.get $pointer))

  (func (export "frame-new") (param $slots i32) (result i32)
    (local $size i64) (local $frame i32) (local $next i32)
    (local.set $size (i64.add (i64.const 12) (i64.mul (i64.extend_i32_u (local.get $slots)) (i64.const 4))))
    (if (i64.gt_u (local.get $size) (i64.const 4294967295)) (then unreachable))
    (local.set $frame (call $allocate (i32.const 4) (i32.wrap_i64 (local.get $size))))
    (memory.fill (local.get $frame) (i32.const 0) (i32.wrap_i64 (local.get $size)))
    (i32.store offset=16 (i32.load (i32.sub (local.get $frame) (i32.const 4))) (i32.const 6))
    (local.set $next (i32.load (i32.const 44)))
    (i32.store (local.get $frame) (local.get $next))
    (i32.store offset=8 (local.get $frame) (local.get $slots))
    (if (local.get $next) (then (i32.store offset=4 (local.get $next) (local.get $frame))))
    (i32.store (i32.const 44) (local.get $frame))
    (local.get $frame))

  (func (export "frame-drop") (param $frame i32)
    (local $next i32) (local $previous i32)
    (local.set $next (i32.load (local.get $frame)))
    (local.set $previous (i32.load offset=4 (local.get $frame)))
    (if (local.get $previous)
      (then (i32.store (local.get $previous) (local.get $next)))
      (else (i32.store (i32.const 44) (local.get $next))))
    (if (local.get $next) (then (i32.store offset=4 (local.get $next) (local.get $previous))))
    (i32.store offset=16 (i32.load (i32.sub (local.get $frame) (i32.const 4))) (i32.const -1)))

  (func $mark (param $pointer i32) (local $block i32)
    (local.set $block (call $find (local.get $pointer)))
    (if (local.get $block) (then
      (if (i32.eqz (i32.load offset=20 (local.get $block)))
        (then (i32.store offset=20 (local.get $block) (i32.const 1)))))))

  (func (export "collect")
    (local $block i32) (local $next i32) (local $previous i32) (local $pointer i32)
    (local $kind i32) (local $changed i32) (local $index i32) (local $count i32)
    ;; Completed native transports no longer need the invocation's pending root.
    (local.set $pointer (i32.load (i32.const 4)))
    (block $promises_done (loop $promises
      (br_if $promises_done (i32.eqz (local.get $pointer)))
      (local.set $next (i32.load offset=16 (local.get $pointer)))
      (if (i32.eq (i32.load (local.get $pointer)) (i32.const 2))
        (then (if (local.get $previous)
          (then (i32.store offset=16 (local.get $previous) (local.get $next)))
          (else (i32.store (i32.const 4) (local.get $next)))))
        (else (call $mark (local.get $pointer)) (local.set $previous (local.get $pointer))))
      (local.set $pointer (local.get $next))
      (br $promises)))
    (local.set $pointer (i32.load (i32.const 44)))
    (block $frames_done (loop $frames
      (br_if $frames_done (i32.eqz (local.get $pointer)))
      (call $mark (local.get $pointer))
      (local.set $pointer (i32.load (local.get $pointer)))
      (br $frames)))
    (loop $trace
      (local.set $changed (i32.const 0))
      (local.set $block (i32.load (i32.const 36)))
      (block $traced (loop $scan
        (br_if $traced (i32.eqz (local.get $block)))
        (if (i32.eq (i32.load offset=20 (local.get $block)) (i32.const 1)) (then
          (i32.store offset=20 (local.get $block) (i32.const 2))
          (local.set $changed (i32.const 1))
          (local.set $pointer (i32.add (local.get $block) (i32.load offset=8 (local.get $block))))
          (local.set $kind (i32.load offset=16 (local.get $block)))
          (if (i32.eq (local.get $kind) (i32.const 1)) (then
            (if (i32.load offset=4 (local.get $pointer)) (then (call $mark (i32.load (local.get $pointer)))))))
          (if (i32.eq (local.get $kind) (i32.const 2)) (then
            (local.set $count (i32.load offset=4 (local.get $pointer)))
            (local.set $pointer (i32.load (local.get $pointer)))
            (local.set $index (i32.const 0))
            (block $strings_done (loop $strings
              (br_if $strings_done (i32.ge_u (local.get $index) (local.get $count)))
              (if (i32.load offset=4 (local.get $pointer)) (then (call $mark (i32.load (local.get $pointer)))))
              (local.set $pointer (i32.add (local.get $pointer) (i32.const 12)))
              (local.set $index (i32.add (local.get $index) (i32.const 1)))
              (br $strings)))))
          (if (i32.or (i32.eq (local.get $kind) (i32.const 3)) (i32.eq (local.get $kind) (i32.const 4))) (then
            (call $mark (i32.load offset=20 (local.get $pointer)))
            ;; The completion ABI stores descriptor addresses as numeric f64 values.
            (if (i32.and (i32.eq (local.get $kind) (i32.const 4)) (i32.eqz (i32.load offset=4 (local.get $pointer))))
              (then (call $mark (i32.trunc_f64_u (f64.load offset=8 (local.get $pointer))))))))
          (if (i32.eq (local.get $kind) (i32.const 5)) (then (call $mark (i32.load offset=4 (local.get $pointer)))))
          (if (i32.eq (local.get $kind) (i32.const 7)) (then (call $mark (i32.load offset=8 (local.get $pointer)))))
          (if (i32.eq (local.get $kind) (i32.const 8)) (then
            (if (i32.load offset=16 (local.get $pointer)) (then (call $mark (i32.load offset=12 (local.get $pointer)))))))
          (if (i32.eq (local.get $kind) (i32.const 9)) (then (call $mark (i32.load (local.get $pointer)))))
          (if (i32.eq (local.get $kind) (i32.const 10)) (then
            (call $mark (i32.load (local.get $pointer)))
            (call $mark (i32.load offset=4 (local.get $pointer)))
            (if (i32.ge_u (i32.load offset=8 (local.get $pointer)) (i32.const 4))
              (then (call $mark (i32.trunc_f64_u (f64.load offset=16 (local.get $pointer))))))))
          (if (i32.eq (local.get $kind) (i32.const 11)) (then
            (if (i32.ge_u (i32.load (local.get $pointer)) (i32.const 4))
              (then (call $mark (i32.trunc_f64_u (f64.load offset=8 (local.get $pointer))))))))
          (if (i32.eq (local.get $kind) (i32.const 6)) (then
            (local.set $count (i32.load offset=8 (local.get $pointer)))
            (local.set $pointer (i32.add (local.get $pointer) (i32.const 12)))
            (local.set $index (i32.const 0))
            (block $slots_done (loop $slots
              (br_if $slots_done (i32.ge_u (local.get $index) (local.get $count)))
              (call $mark (i32.load (local.get $pointer)))
              (local.set $pointer (i32.add (local.get $pointer) (i32.const 4)))
              (local.set $index (i32.add (local.get $index) (i32.const 1)))
              (br $slots)))))))
        (local.set $block (i32.load (local.get $block)))
        (br $scan)))
      (br_if $trace (local.get $changed)))
    (local.set $block (i32.load (i32.const 36)))
    (block $swept (loop $sweep
      (br_if $swept (i32.eqz (local.get $block)))
      (if (i32.eqz (i32.load offset=20 (local.get $block)))
        (then (i32.store offset=16 (local.get $block) (i32.const -1))))
      (i32.store offset=20 (local.get $block) (i32.const 0))
      (local.set $block (i32.load (local.get $block)))
      (br $sweep)))
    (local.set $block (i32.load (i32.const 36)))
    (block $coalesced (loop $coalesce
      (br_if $coalesced (i32.eqz (local.get $block)))
      (local.set $next (i32.load (local.get $block)))
      (if (local.get $next) (then
        (if (i32.and (i32.eq (i32.load offset=16 (local.get $block)) (i32.const -1))
          (i32.eq (i32.load offset=16 (local.get $next)) (i32.const -1))) (then
            (i32.store (local.get $block) (i32.load (local.get $next)))
            (i32.store offset=4 (local.get $block) (i32.sub (i32.add (local.get $next) (i32.load offset=4 (local.get $next))) (local.get $block)))
            (if (i32.eq (i32.load (i32.const 40)) (local.get $next)) (then (i32.store (i32.const 40) (local.get $block))))
            (br $coalesce)))))
      (local.set $block (local.get $next))
      (br $coalesce)))))
