  ;; Returns the transferred prefix. A short prefix means the reader closed.
  (func $write-buffer (param $stream i32) (param $data i32) (param $length i32) (result i32)
    (local $offset i32) (local $requested i32) (local $status i32) (local $count i32)
    (block $done
      (loop $next
        (br_if $done (i32.eq (local.get $offset) (local.get $length)))
        (local.set $requested (i32.sub (local.get $length) (local.get $offset)))
        (if (i32.gt_u (local.get $requested) (i32.const 65536))
          (then (local.set $requested (i32.const 65536))))
        (local.set $status (call $write (local.get $stream)
          (i32.add (local.get $data) (local.get $offset)) (local.get $requested)))
        ;; Synchronous writes suspend instead of returning BLOCKED or CANCELLED.
        (if (i32.gt_u (i32.and (local.get $status) (i32.const 15)) (i32.const 1)) (then unreachable))
        (local.set $count (i32.shr_u (local.get $status) (i32.const 4)))
        (if (i32.gt_u (local.get $count) (local.get $requested)) (then unreachable))
        (local.set $offset (i32.add (local.get $offset) (local.get $count)))
        (br_if $done (i32.and (local.get $status) (i32.const 1)))
        (br $next)))
    (local.get $offset))
