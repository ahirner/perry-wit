  ;; Returns a transferred element count and whether the readable end closed.
  (func $read-transfer (param $stream i32) (param $data i32) (param $capacity i32) (result i32 i32)
    (local $status i32) (local $length i32) (local $closed i32)
    (if (i32.eqz (local.get $capacity)) (then (return (i32.const 0) (i32.const 0))))
    (loop $read-again
      (local.set $status (call $read (local.get $stream) (local.get $data) (local.get $capacity)))
      ;; Synchronous canonical reads suspend; BLOCKED/CANCELLED cannot be returned.
      (if (i32.gt_u (i32.and (local.get $status) (i32.const 15)) (i32.const 1)) (then unreachable))
      (local.set $length (i32.shr_u (local.get $status) (i32.const 4)))
      (if (i32.gt_u (local.get $length) (local.get $capacity)) (then unreachable))
      (local.set $closed (i32.and (local.get $status) (i32.const 1)))
      (br_if $read-again (i32.and (i32.eqz (local.get $length)) (i32.eqz (local.get $closed)))))
    (local.get $length) (local.get $closed))
