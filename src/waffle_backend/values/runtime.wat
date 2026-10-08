(module
  (import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
  (import "host" "compare" (func $compare (param i32 i32) (result i32)))
  (memory 1)
  ;; Immutable value: tag, padding, f64 payload. References use numeric pointers.
  (func $value.new (export "value.new") (param $tag i32) (param $payload f64) (result i32)
    (local $value i32)
    (local.set $value (call $realloc (i32.const 0) (i32.const 0) (i32.const 8) (i32.const 16)))
    (i32.store offset=16 (i32.load (i32.sub (local.get $value) (i32.const 4))) (i32.const 11))
    (i32.store (local.get $value) (local.get $tag))
    (f64.store offset=8 (local.get $value) (local.get $payload))
    (local.get $value))

  (func $extract (export "value.extract") (param $value i32) (param $tag i32) (result i32 f64)
    (if (i32.ne (i32.load (local.get $value)) (local.get $tag))
      (then (return (i32.const 1) (f64.const 12))))
    (i32.const 0) (f64.load offset=8 (local.get $value)))

  (func (export "value.extract-optional") (param $value i32) (param $tag i32) (result i32 f64)
    (if (i32.eqz (i32.load (local.get $value))) (then
      (return (i32.const 0)
        (select (f64.const nan) (f64.const 0) (i32.eq (local.get $tag) (i32.const 3))))))
    (call $extract (local.get $value) (local.get $tag)))

  (func (export "value.truthy") (param $value i32) (result i32)
    (local $tag i32) (local $payload f64)
    (local.set $tag (i32.load (local.get $value)))
    (if (i32.le_u (local.get $tag) (i32.const 1)) (then (return (i32.const 0))))
    (local.set $payload (f64.load offset=8 (local.get $value)))
    (if (i32.le_u (local.get $tag) (i32.const 3)) (then
      (return (i32.and (f64.ne (local.get $payload) (f64.const 0)) (f64.eq (local.get $payload) (local.get $payload))))))
    (if (i32.eq (local.get $tag) (i32.const 4)) (then
      (return (i32.ne (i32.load offset=4 (i32.trunc_f64_u (local.get $payload))) (i32.const 0)))))
    (i32.const 1))

  (func (export "value.equal") (param $tag i32) (param $left f64) (param $right_tag i32) (param $right f64) (result i32)
    (if (i32.ne (local.get $tag) (local.get $right_tag)) (then (return (i32.const 0))))
    (if (i32.le_u (local.get $tag) (i32.const 1)) (then (return (i32.const 1))))
    (if (i32.eq (local.get $tag) (i32.const 4)) (then
      (return (i32.eqz (call $compare (i32.trunc_f64_u (local.get $left))
        (i32.trunc_f64_u (local.get $right)))))))
    (f64.eq (local.get $left) (local.get $right)))

  (func $scalar-number (export "value.scalar-number") (param $value i32) (result i32 f64)
    (local $tag i32)
    (local.set $tag (i32.load (local.get $value)))
    (if (i32.eqz (local.get $tag)) (then (return (i32.const 0) (f64.const nan))))
    (if (i32.eq (local.get $tag) (i32.const 1)) (then (return (i32.const 0) (f64.const 0))))
    (if (i32.le_u (local.get $tag) (i32.const 3)) (then
      (return (i32.const 0) (f64.load offset=8 (local.get $value)))))
    (i32.const 1) (f64.const 12))

  ;; Dynamic Promise adoption requires an outcome tag, beyond an opaque Promise handle.
  (func (export "value.async-result") (param $value i32) (result i32 f64)
    (if (i32.eq (i32.load (local.get $value)) (i32.const 10))
      (then (return (i32.const 1) (f64.const 12))))
    (i32.const 0) (f64.convert_i32_u (local.get $value)))

  (func (export "value.exception") (param $status i32) (param $payload f64) (result f64)
    (if (i32.eq (local.get $status) (i32.const 3)) (then (return (local.get $payload))))
    (f64.convert_i32_u (call $value.new (i32.const 3) (local.get $payload))))

  ;; The fixture-only Result<T, number> boundary accepts numeric thrown values.
  (func (export "value.exception-number") (param $status i32) (param $payload f64) (result f64)
    (local $value i32)
    (if (i32.eq (local.get $status) (i32.const 1)) (then (return (local.get $payload))))
    (local.set $value (i32.trunc_f64_u (local.get $payload)))
    (if (i32.ne (i32.load (local.get $value)) (i32.const 3)) (then unreachable))
    (f64.load offset=8 (local.get $value)))
)
