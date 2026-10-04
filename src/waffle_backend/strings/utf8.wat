  ;; Returns complete prefix length, scalar count, and the unconsumed offset on
  ;; failure (-1 on success). An invalid continuation is restored for the next call.
  (func $scan (param $data i32) (param $length i32) (param $stream i32) (result i32 i32 i32)
    (local $index i32) (local $scalars i32) (local $lead i32) (local $width i32)
    (local $next i32) (local $byte i32) (local $lower i32) (local $upper i32)
    (block $done (loop $scalar
      (br_if $done (i32.ge_u (local.get $index) (local.get $length)))
      (local.set $lead (i32.load8_u (i32.add (local.get $data) (local.get $index))))
      (local.set $width (i32.const 1))
      (if (i32.ge_u (local.get $lead) (i32.const 128)) (then
        (if (i32.or (i32.lt_u (local.get $lead) (i32.const 194)) (i32.gt_u (local.get $lead) (i32.const 244)))
          (then (return (local.get $index) (local.get $scalars) (i32.add (local.get $index) (i32.const 1)))))
        (local.set $width (i32.const 2))
        (if (i32.ge_u (local.get $lead) (i32.const 224)) (then (local.set $width (i32.const 3))))
        (if (i32.ge_u (local.get $lead) (i32.const 240)) (then (local.set $width (i32.const 4))))))
      (local.set $next (i32.const 1))
      (block $complete (loop $continuation
        (br_if $complete (i32.ge_u (local.get $next) (local.get $width)))
        (if (i32.ge_u (local.get $next) (i32.sub (local.get $length) (local.get $index))) (then
          (if (local.get $stream)
            (then (return (local.get $index) (local.get $scalars) (i32.const -1)))
            (else (return (local.get $index) (local.get $scalars) (local.get $length))))))
        (local.set $byte (i32.load8_u (i32.add (local.get $data) (i32.add (local.get $index) (local.get $next)))))
        (local.set $lower (i32.const 128))
        (local.set $upper (i32.const 191))
        (if (i32.eq (local.get $next) (i32.const 1)) (then
          (if (i32.eq (local.get $lead) (i32.const 224)) (then (local.set $lower (i32.const 160))))
          (if (i32.eq (local.get $lead) (i32.const 237)) (then (local.set $upper (i32.const 159))))
          (if (i32.eq (local.get $lead) (i32.const 240)) (then (local.set $lower (i32.const 144))))
          (if (i32.eq (local.get $lead) (i32.const 244)) (then (local.set $upper (i32.const 143))))))
        (if (i32.or (i32.lt_u (local.get $byte) (local.get $lower)) (i32.gt_u (local.get $byte) (local.get $upper)))
          (then (return (local.get $index) (local.get $scalars) (i32.add (local.get $index) (local.get $next)))))
        (local.set $next (i32.add (local.get $next) (i32.const 1)))
        (br $continuation)))
      (local.set $scalars (i32.add (local.get $scalars) (i32.const 1)))
      (local.set $index (i32.add (local.get $index) (local.get $width)))
      (br $scalar)))
    (local.get $index) (local.get $scalars) (i32.const -1))
