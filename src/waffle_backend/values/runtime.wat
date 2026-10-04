(module
  (import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
  (import "host" "compare" (func $compare (param i32 i32) (result i32)))
  (memory 1)
  ;; Immutable value: tag, padding, f64 payload. References use numeric pointers.
  (func (export "value.new") (param $tag i32) (param $payload f64) (result i32)
    (local $value i32)
    (local.set $value (call $realloc (i32.const 0) (i32.const 0) (i32.const 8) (i32.const 16)))
    (i32.store offset=16 (i32.load (i32.sub (local.get $value) (i32.const 4))) (i32.const 11))
    (i32.store (local.get $value) (local.get $tag))
    (f64.store offset=8 (local.get $value) (local.get $payload))
    (local.get $value))

  (func (export "value.extract") (param $value i32) (param $tag i32) (result i32 f64)
    (if (i32.ne (i32.load (local.get $value)) (local.get $tag))
      (then (return (i32.const 1) (f64.const 12))))
    (i32.const 0) (f64.load offset=8 (local.get $value)))

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

  (func (export "value.equal") (param $left i32) (param $right i32) (result i32)
    (local $tag i32)
    (local.set $tag (i32.load (local.get $left)))
    (if (i32.ne (local.get $tag) (i32.load (local.get $right))) (then (return (i32.const 0))))
    (if (i32.le_u (local.get $tag) (i32.const 1)) (then (return (i32.const 1))))
    (if (i32.eq (local.get $tag) (i32.const 4)) (then
      (return (i32.eqz (call $compare (i32.trunc_f64_u (f64.load offset=8 (local.get $left)))
        (i32.trunc_f64_u (f64.load offset=8 (local.get $right))))))))
    (f64.eq (f64.load offset=8 (local.get $left)) (f64.load offset=8 (local.get $right))))

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
)
