(module
  (import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
  (import "host" "lift" (func $lift (param i32 i32) (result i32)))
  (import "host" "serialize" (func $serialize (param i32 i32 i32 i32 i32) (result i64)))
  (memory 1)

  ;; Scratch contains one number node, its 32-byte output, and 128 ancestor pointers.
  ;; The traversal never calls guest code or collects; both passes see the same values.
  (func (export "value.to-string") (param $tag i32) (param $value f64) (result i32 f64)
    (local $scratch i32) (local $status i32) (local $length i32) (local $output i32)
    (if (i32.eq (local.get $tag) (i32.const 4)) (then (return (i32.const 0) (local.get $value))))
    (local.set $scratch (call $realloc (i32.const 0) (i32.const 0) (i32.const 8) (i32.const 576)))
    (memory.fill (local.get $scratch) (i32.const 0) (i32.const 32))
    (i32.store (local.get $scratch) (i32.const 3))
    (call $walk (local.get $tag) (local.get $value) (local.get $scratch) (i32.const 0) (i32.const 0) (i32.const 0))
    local.set $length local.set $status
    (if (local.get $status) (then
      (drop (call $realloc (local.get $scratch) (i32.const 576) (i32.const 8) (i32.const 0)))
      (return (i32.const 1) (f64.convert_i32_u (local.get $status)))))
    (local.set $output (call $realloc (i32.const 0) (i32.const 0) (i32.const 1) (local.get $length)))
    (call $walk (local.get $tag) (local.get $value) (local.get $scratch) (local.get $output) (i32.const 0) (i32.const 0))
    drop local.set $status
    (if (local.get $status) (then unreachable))
    (drop (call $realloc (local.get $scratch) (i32.const 576) (i32.const 8) (i32.const 0)))
    (i32.const 0) (f64.convert_i32_u (call $lift (local.get $output) (local.get $length))))

  (func $walk (param $tag i32) (param $value f64) (param $scratch i32) (param $output i32) (param $cursor i32) (param $depth i32) (result i32 i32)
    (local $text i32) (local $pointer i32) (local $data i32) (local $count i32)
    (local $index i32) (local $item i32) (local $item-tag i32) (local $item-value f64)
    (local $status i32) (local $result i64)
    (if (i32.eqz (local.get $tag)) (then (local.set $text (i32.const {{undefined}}))))
    (if (i32.eq (local.get $tag) (i32.const 1)) (then (local.set $text (i32.const {{null}}))))
    (if (i32.eq (local.get $tag) (i32.const 2)) (then
      (local.set $text (select (i32.const {{true}}) (i32.const {{false}}) (f64.ne (local.get $value) (f64.const 0))))))
    (if (i32.eq (local.get $tag) (i32.const 3)) (then
      (if (f64.ne (local.get $value) (local.get $value)) (then (local.set $text (i32.const {{NaN}}))))
      (if (f64.eq (local.get $value) (f64.const inf)) (then (local.set $text (i32.const {{Infinity}}))))
      (if (f64.eq (local.get $value) (f64.const -inf)) (then (local.set $text (i32.const {{-Infinity}}))))
      (if (i32.eqz (local.get $text)) (then
        (f64.store offset=8 (local.get $scratch) (local.get $value))
        (local.set $data (i32.add (local.get $scratch) (i32.const 32)))
        (local.set $result (call $serialize (local.get $scratch) (i32.const 32) (local.get $scratch) (local.get $data) (i32.const 32)))
        (if (i64.ne (i64.shr_u (local.get $result) (i64.const 32)) (i64.const 0)) (then unreachable))
        (return (i32.const 0) (call $append (local.get $output) (local.get $cursor) (local.get $data) (i32.wrap_i64 (local.get $result))))))))
    (if (i32.eq (local.get $tag) (i32.const 4)) (then (local.set $text (i32.trunc_f64_u (local.get $value)))))
    (if (i32.eq (local.get $tag) (i32.const 6)) (then (local.set $text (i32.const {{[object Object]}}))))
    (if (local.get $text) (then
      (return (i32.const 0) (call $append (local.get $output) (local.get $cursor) (i32.load (local.get $text)) (i32.load offset=4 (local.get $text))))))
    (if (i32.eqz (i32.or (i32.eq (local.get $tag) (i32.const 8)) (i32.eq (local.get $tag) (i32.const 12))))
      (then (return (i32.const 12) (local.get $cursor))))
    (local.set $pointer (i32.trunc_f64_u (local.get $value)))
    (block $checked (loop $ancestors
      (br_if $checked (i32.ge_u (local.get $index) (local.get $depth)))
      (if (i32.eq (local.get $pointer) (i32.load offset=64 (i32.add (local.get $scratch) (i32.mul (local.get $index) (i32.const 4)))))
        (then (return (i32.const 0) (local.get $cursor))))
      (local.set $index (i32.add (local.get $index) (i32.const 1)))
      (br $ancestors)))
    (if (i32.ge_u (local.get $depth) (i32.const 128)) (then (return (i32.const 3) (local.get $cursor))))
    (i32.store offset=64 (i32.add (local.get $scratch) (i32.mul (local.get $depth) (i32.const 4))) (local.get $pointer))
    (local.set $count (i32.load offset=4 (local.get $pointer)))
    (local.set $data (i32.load (local.get $pointer)))
    (local.set $index (i32.const 0))
    (block $done (loop $elements
      (br_if $done (i32.ge_u (local.get $index) (local.get $count)))
      (if (local.get $index) (then
        (if (i32.eq (local.get $cursor) (i32.const -1)) (then unreachable))
        (if (local.get $output) (then (i32.store8 (i32.add (local.get $output) (local.get $cursor)) (i32.const 44))))
        (local.set $cursor (i32.add (local.get $cursor) (i32.const 1)))))
      (if (i32.eq (local.get $tag) (i32.const 8))
        (then
          (local.set $item-tag (i32.const 4))
          (local.set $item-value (f64.convert_i32_u (i32.add (local.get $data) (i32.mul (local.get $index) (i32.const 12))))))
        (else
          (local.set $item (i32.load (i32.add (local.get $data) (i32.mul (local.get $index) (i32.const 4)))))
          (local.set $item-tag (i32.const 0))
          (if (local.get $item) (then
            (local.set $item-tag (i32.load (local.get $item)))
            (local.set $item-value (f64.load offset=8 (local.get $item)))))))
      (if (i32.gt_u (local.get $item-tag) (i32.const 1)) (then
        (call $walk (local.get $item-tag) (local.get $item-value) (local.get $scratch) (local.get $output) (local.get $cursor) (i32.add (local.get $depth) (i32.const 1)))
        local.set $cursor local.set $status
        (if (local.get $status) (then (return (local.get $status) (local.get $cursor))))))
      (local.set $index (i32.add (local.get $index) (i32.const 1)))
      (br $elements)))
    (i32.const 0) (local.get $cursor))

  (func $append (param $output i32) (param $cursor i32) (param $data i32) (param $length i32) (result i32)
    (local $end i64)
    (local.set $end (i64.add (i64.extend_i32_u (local.get $cursor)) (i64.extend_i32_u (local.get $length))))
    (if (i64.gt_u (local.get $end) (i64.const 4294967295)) (then unreachable))
    (if (local.get $output) (then
      (memory.copy (i32.add (local.get $output) (local.get $cursor)) (local.get $data) (local.get $length))))
    (i32.wrap_i64 (local.get $end)))
)
