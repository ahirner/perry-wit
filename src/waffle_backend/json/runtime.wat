(module
  (import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
  (import "host" "date-iso" (func $date-iso (param i32) (result i32 f64)))
  (import "host" "array-new" (func $array-new (param i32) (result i32)))
  (import "host" "box" (func $box (param i32 f64) (result i32)))
  (import "host" "lift" (func $lift (param i32 i32) (result i32)))
  (import "host" "object-new" (func $object-new (result i32)))
  (import "host" "object-set" (func $object-set (param i32 i32 i32 f64) (result i32 f64)))
  (import "host" "measure" (func $measure (param i32 i32) (result i64)))
  (import "host" "populate" (func $populate (param i32 i32 i32 i32) (result i64)))
  (import "host" "serialized-size" (func $serialized-size (param i32 i32 i32) (result i64)))
  (import "host" "serialize" (func $serialize (param i32 i32 i32 i32 i32) (result i64)))
  (import "host" "error" (func $failure (param i64 i32) (result i32 f64)))
  (memory 1)

  (func $allocate (param $size i32) (result i32)
    (call $realloc (i32.const 0) (i32.const 0) (i32.const 8) (local.get $size)))
  (func $release (param $pointer i32) (param $size i32)
    (drop (call $realloc (local.get $pointer) (local.get $size) (i32.const 8) (i32.const 0))))
  (func $error (param $result i64) (result i32)
    (i32.wrap_i64 (i64.shr_u (local.get $result) (i64.const 32))))

  ;; Conversion only reads graphs validated by the Rust codec. Strings retain
  ;; their graph allocation through ordinary traced string descriptors.
  (func $from-node (param $node i32) (result i32)
    (local $kind i32) (local $value i32) (local $child i32) (local $item i32)
    (local $count i32) (local $index i32) (local $key i32)
    (local.set $kind (i32.load (local.get $node)))
    (if (i32.eq (local.get $kind) (i32.const 1))
      (then (return (call $box (i32.const 1) (f64.const 0)))))
    (if (i32.eq (local.get $kind) (i32.const 2))
      (then (return (call $box (i32.const 2) (f64.convert_i32_u (i32.load offset=8 (local.get $node)))))))
    (if (i32.eq (local.get $kind) (i32.const 3))
      (then (return (call $box (i32.const 3) (f64.load offset=8 (local.get $node))))))
    (if (i32.eq (local.get $kind) (i32.const 4)) (then
      (local.set $value (call $lift (i32.load offset=8 (local.get $node)) (i32.load offset=12 (local.get $node))))
      (return (call $box (i32.const 4) (f64.convert_i32_u (local.get $value))))))
    (local.set $count (i32.load offset=20 (local.get $node)))
    (local.set $child (i32.load offset=16 (local.get $node)))
    (if (i32.eq (local.get $kind) (i32.const 5)) (then
      (local.set $value (call $array-new (local.get $count))))
      (else (local.set $value (call $object-new))))
    (block $done (loop $children
      (br_if $done (i32.ge_u (local.get $index) (local.get $count)))
      (local.set $item (call $from-node (local.get $child)))
      (if (i32.eq (local.get $kind) (i32.const 5))
        (then (i32.store (i32.add (i32.load (local.get $value)) (i32.mul (local.get $index) (i32.const 4))) (local.get $item)))
        (else
          (local.set $key (i32.load offset=24 (local.get $child)))
          (local.set $key (call $lift (i32.load offset=8 (local.get $key)) (i32.load offset=12 (local.get $key))))
          (call $object-set (local.get $value) (local.get $key) (i32.load (local.get $item)) (f64.load offset=8 (local.get $item)))
          drop drop))
      (local.set $child (i32.load offset=4 (local.get $child)))
      (local.set $index (i32.add (local.get $index) (i32.const 1)))
      (br $children)))
    (call $box (select (i32.const {{array-tag}}) (i32.const 6) (i32.eq (local.get $kind) (i32.const 5))) (f64.convert_i32_u (local.get $value))))

  (func (export "json.parse") (param $text i32) (result i32 f64)
    (local $result i64) (local $size i32) (local $graph i32) (local $code i32)
    (local.set $result (call $measure (i32.load (local.get $text)) (i32.load offset=4 (local.get $text))))
    (local.set $code (call $error (local.get $result)))
    (if (local.get $code) (then (return (call $failure (local.get $result) (local.get $text)))))
    (local.set $size (i32.wrap_i64 (local.get $result)))
    (local.set $graph (call $allocate (local.get $size)))
    (local.set $result (call $populate (i32.load (local.get $text)) (i32.load offset=4 (local.get $text)) (local.get $graph) (local.get $size)))
    (local.set $code (call $error (local.get $result)))
    (if (local.get $code) (then
      (call $release (local.get $graph) (local.get $size))
      (return (call $failure (local.get $result) (local.get $text)))))
    (i32.const 0) (f64.convert_i32_u (call $from-node (i32.wrap_i64 (local.get $result)))))

  ;; The same traversal measures and writes a confined graph. No guest callbacks
  ;; or collection occur between these passes. Depth bounds also reject cycles.
  (func $graph (param $tag i32) (param $value f64) (param $output i32) (param $cursor i32) (param $depth i32) (result i32 i32 i32)
    (local $node i32) (local $end i32) (local $pointer i32) (local $length i32)
    (local $child i32) (local $last i32) (local $count i32) (local $index i32)
    (local $item i32) (local $key i32) (local $status i32) (local $key-node i32)
    (local $item-tag i32) (local $item-value f64) (local $size i64)
    (if (i32.ge_u (local.get $depth) (i32.const 128))
      (then (return (i32.const 3) (i32.const 0) (local.get $cursor))))
    (if (i32.eq (local.get $tag) (i32.const 11)) (then
      (local.set $pointer (i32.trunc_f64_u (local.get $value)))
      (local.set $item-value (f64.load (local.get $pointer)))
      (if (f64.ne (local.get $item-value) (local.get $item-value))
        (then (return (call $graph (i32.const 1) (f64.const 0) (local.get $output) (local.get $cursor) (local.get $depth)))))
      (call $date-iso (local.get $pointer)) local.set $item-value local.set $status
      (if (local.get $status) (then (return (local.get $status) (i32.const 0) (local.get $cursor))))
      (return (call $graph (i32.const 4) (local.get $item-value) (local.get $output) (local.get $cursor) (local.get $depth)))))
    (local.set $size (i64.and (i64.add (i64.extend_i32_u (local.get $cursor)) (i64.const 7)) (i64.const -8)))
    (local.set $size (i64.add (local.get $size) (i64.const 32)))
    (if (i64.gt_u (local.get $size) (i64.const 4294967295)) (then unreachable))
    (local.set $end (i32.wrap_i64 (local.get $size)))
    (local.set $node (i32.add (local.get $output) (i32.sub (local.get $end) (i32.const 32))))
    (if (local.get $output) (then
      (memory.fill (local.get $node) (i32.const 0) (i32.const 32))
      (i32.store (local.get $node) (select (i32.const 1) (local.get $tag) (i32.eqz (local.get $tag))))))
    (if (i32.le_u (local.get $tag) (i32.const 3)) (then
      (if (local.get $output) (then
        (if (i32.eq (local.get $tag) (i32.const 2))
          (then (i32.store offset=8 (local.get $node) (f64.ne (local.get $value) (f64.const 0))))
          (else (f64.store offset=8 (local.get $node) (local.get $value))))))
      (return (i32.const 0) (local.get $node) (local.get $end))))
    (local.set $pointer (i32.trunc_f64_u (local.get $value)))
    (if (i32.eq (local.get $tag) (i32.const 4)) (then
      (local.set $length (i32.load offset=4 (local.get $pointer)))
      (local.set $size (i64.add (i64.extend_i32_u (local.get $end)) (i64.extend_i32_u (local.get $length))))
      (if (i64.gt_u (local.get $size) (i64.const 4294967295)) (then unreachable))
      (if (local.get $output) (then
        (i32.store offset=8 (local.get $node) (i32.add (local.get $output) (local.get $end)))
        (i32.store offset=12 (local.get $node) (local.get $length))
        (i32.store offset=16 (local.get $node) (i32.load offset=8 (local.get $pointer)))
        (memory.copy (i32.add (local.get $output) (local.get $end)) (i32.load (local.get $pointer)) (local.get $length))))
      (return (i32.const 0) (local.get $node) (i32.wrap_i64 (local.get $size)))))
    (if (i32.eq (local.get $tag) (i32.const 6))
      (then (local.set $item (i32.load (local.get $pointer))))
      (else
        (if (i32.eqz (i32.or (i32.eq (local.get $tag) (i32.const 8)) (i32.eq (local.get $tag) (i32.const {{array-tag}}))))
          (then (return (i32.const 5) (i32.const 0) (local.get $end))))
        (local.set $length (i32.load offset=4 (local.get $pointer)))
        (local.set $pointer (i32.load (local.get $pointer)))
        (if (local.get $output) (then (i32.store (local.get $node) (i32.const 5))))))
    (block $done (loop $children
      (if (i32.eq (local.get $tag) (i32.const 6))
        (then
          (br_if $done (i32.eqz (local.get $item)))
          (if (i32.load offset=12 (local.get $item)) (then
            (local.set $item (i32.load (local.get $item))) (br $children)))
          (local.set $item-tag (i32.load offset=8 (local.get $item)))
          (local.set $item-value (f64.load offset=16 (local.get $item)))
          (local.set $key (i32.load offset=4 (local.get $item)))
          (local.set $item (i32.load (local.get $item)))
          (br_if $children (i32.eqz (local.get $item-tag))))
        (else
          (br_if $done (i32.ge_u (local.get $index) (local.get $length)))
          (if (i32.eq (local.get $tag) (i32.const 8))
            (then
              (local.set $item-tag (i32.const 4))
              (local.set $item-value (f64.convert_i32_u (i32.add (local.get $pointer) (i32.mul (local.get $index) (i32.const 12))))))
            (else
              (local.set $item (i32.load (i32.add (local.get $pointer) (i32.mul (local.get $index) (i32.const 4)))))
              (local.set $item-tag (i32.const 0)) (local.set $item-value (f64.const 0))
              (if (local.get $item) (then
                (local.set $item-tag (i32.load (local.get $item)))
                (local.set $item-value (f64.load offset=8 (local.get $item)))))))
          (local.set $index (i32.add (local.get $index) (i32.const 1)))))
      (call $graph (local.get $item-tag) (local.get $item-value) (local.get $output) (local.get $end) (i32.add (local.get $depth) (i32.const 1)))
      local.set $end local.set $child local.set $status
      (if (local.get $status) (then (return (local.get $status) (local.get $node) (local.get $end))))
      (if (i32.eq (local.get $tag) (i32.const 6)) (then
        (call $graph (i32.const 4) (f64.convert_i32_u (local.get $key)) (local.get $output) (local.get $end) (local.get $depth))
        local.set $end local.set $key-node local.set $status
        (if (local.get $status) (then (return (local.get $status) (local.get $node) (local.get $end))))
        (if (local.get $output) (then (i32.store offset=24 (local.get $child) (local.get $key-node))))))
      (if (local.get $output) (then
        (if (local.get $count)
          (then (i32.store offset=4 (local.get $last) (local.get $child)))
          (else (i32.store offset=16 (local.get $node) (local.get $child))))))
      (local.set $last (local.get $child))
      (local.set $count (i32.add (local.get $count) (i32.const 1)))
      (br $children)))
    (if (local.get $output) (then (i32.store offset=20 (local.get $node) (local.get $count))))
    (i32.const 0) (local.get $node) (local.get $end))

  (func (export "json.stringify") (param $value i32) (result i32 f64)
    (local $graph i32) (local $size i32) (local $root i32) (local $status i32)
    (local $result i64) (local $output i32) (local $length i32) (local $end i32)
    (if (i32.eqz (i32.load (local.get $value))) (then (return (i32.const 0) (f64.const 0))))
    (call $graph (i32.load (local.get $value)) (f64.load offset=8 (local.get $value)) (i32.const 0) (i32.const 0) (i32.const 0))
    local.set $size local.set $root local.set $status
    (if (local.get $status) (then (return (call $failure
      (i64.shl (i64.extend_i32_u (local.get $status)) (i64.const 32)) (i32.const 0)))))
    (local.set $graph (call $allocate (local.get $size)))
    (call $graph (i32.load (local.get $value)) (f64.load offset=8 (local.get $value)) (local.get $graph) (i32.const 0) (i32.const 0))
    local.set $end local.set $root local.set $status
    (if (local.get $status) (then unreachable))
    (local.set $result (call $serialized-size (local.get $graph) (local.get $size) (local.get $root)))
    (local.set $status (call $error (local.get $result)))
    (if (local.get $status) (then
      (call $release (local.get $graph) (local.get $size))
      (return (call $failure (local.get $result) (i32.const 0)))))
    (local.set $length (i32.wrap_i64 (local.get $result)))
    (local.set $output (call $allocate (local.get $length)))
    (local.set $result (call $serialize (local.get $graph) (local.get $size) (local.get $root) (local.get $output) (local.get $length)))
    (call $release (local.get $graph) (local.get $size))
    (local.set $status (call $error (local.get $result)))
    (if (local.get $status) (then
      (call $release (local.get $output) (local.get $length))
      (return (call $failure (local.get $result) (i32.const 0)))))
    (i32.const 0) (f64.convert_i32_u (call $lift (local.get $output) (local.get $length))))
)
