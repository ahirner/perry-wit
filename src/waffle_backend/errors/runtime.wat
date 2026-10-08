(module
  (import "host" "record" (func $record (param i32) (result i32)))
  (import "host" "set" (func $set (param i32 i32 i32 f64) (result i32 f64)))
  (import "host" "get" (func $get (param i32 i32) (result i32)))
  (import "host" "dynamic" (func $dynamic (param i32) (result i32)))
  (import "host" "box" (func $box (param i32 f64) (result i32)))
  (import "host" "concat" (func $concat (param i32 i32) (result i32)))
  (import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
  (import "host" "lift" (func $lift (param i32 i32) (result i32)))
  (import "host" "number-format" (func $number-format (param f64 i32) (result i32)))
  (memory 1)

  (func $hidden (param $object i32) (param $key i32) (param $tag i32) (param $payload f64)
    (call $set (local.get $object) (local.get $key) (local.get $tag) (local.get $payload)) drop drop
    (i32.store offset=12 (call $get (local.get $object) (local.get $key)) (i32.const 1)))

  (func $new (export "error.new") (param $kind i32) (param $message i32) (param $cause i32) (result i32)
    (local $object i32) (local $name i32)
    (local.set $object (call $record (i32.const 3)))
    (i32.store offset=20 (local.get $object) (i32.add (local.get $kind) (i32.const 1)))
    (local.set $name (i32.const {{Error}}))
    (if (i32.eq (local.get $kind) (i32.const 1)) (then (local.set $name (i32.const {{TypeError}}))))
    (if (i32.eq (local.get $kind) (i32.const 2)) (then (local.set $name (i32.const {{RangeError}}))))
    (if (i32.eq (local.get $kind) (i32.const 3)) (then (local.set $name (i32.const {{ReferenceError}}))))
    (if (i32.eq (local.get $kind) (i32.const 4)) (then (local.set $name (i32.const {{SyntaxError}}))))
    (call $hidden (local.get $object) (i32.const {{name}}) (i32.const 4) (f64.convert_i32_u (local.get $name)))
    (call $hidden (local.get $object) (i32.const {{message}}) (i32.const 4) (f64.convert_i32_u (local.get $message)))
    (if (local.get $cause) (then
      (call $hidden (local.get $object) (i32.const {{cause}})
        (i32.load (local.get $cause)) (f64.load offset=8 (local.get $cause)))))
    (call $box (i32.const 6) (f64.convert_i32_u (local.get $object))))

  (func (export "error.cause") (param $options i32) (result i32)
    (local $entry i32)
    (if (i32.ne (i32.load (local.get $options)) (i32.const 6)) (then (return (i32.const 0))))
    (local.set $entry (call $get (i32.trunc_f64_u (f64.load offset=8 (local.get $options))) (i32.const {{cause}})))
    (if (i32.eqz (local.get $entry)) (then (return (i32.const 0))))
    (call $dynamic (local.get $entry)))

  (func (export "error.is") (param $value i32) (param $kind i32) (result i32)
    (local $brand i32)
    (if (i32.ne (i32.load (local.get $value)) (i32.const 6)) (then (return (i32.const 0))))
    (local.set $brand (i32.load offset=20 (i32.trunc_f64_u (f64.load offset=8 (local.get $value)))))
    (if (i32.eqz (local.get $kind)) (then (return (i32.ne (local.get $brand) (i32.const 0)))))
    (i32.eq (local.get $brand) (i32.add (local.get $kind) (i32.const 1))))

  ;; Formatting never invokes user code or recursively serializes arbitrary objects.
  (func $describe (export "error.describe") (param $value i32) (result i32)
    (local $tag i32) (local $payload f64) (local $object i32)
    (local $name i32) (local $message i32) (local $buffer i32)
    (local.set $tag (i32.load (local.get $value)))
    (local.set $payload (f64.load offset=8 (local.get $value)))
    (if (i32.eqz (local.get $tag)) (then (return (i32.const {{undefined}}))))
    (if (i32.eq (local.get $tag) (i32.const 1)) (then (return (i32.const {{null}}))))
    (if (i32.eq (local.get $tag) (i32.const 2)) (then
      (return (select (i32.const {{true}}) (i32.const {{false}}) (f64.ne (local.get $payload) (f64.const 0))))))
    (if (i32.eq (local.get $tag) (i32.const 3)) (then
      (local.set $buffer (call $realloc (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 40)))
      (return (call $lift (local.get $buffer) (call $number-format (local.get $payload) (local.get $buffer))))))
    (if (i32.eq (local.get $tag) (i32.const 4)) (then (return (i32.trunc_f64_u (local.get $payload)))))
    (if (i32.eq (local.get $tag) (i32.const 6)) (then
      (local.set $object (i32.trunc_f64_u (local.get $payload)))
      (if (i32.load offset=20 (local.get $object)) (then
        (local.set $name (call $get (local.get $object) (i32.const {{name}})))
        (local.set $message (call $get (local.get $object) (i32.const {{message}})))
        (if (i32.and (i32.eq (i32.load offset=8 (local.get $name)) (i32.const 4))
                     (i32.eq (i32.load offset=8 (local.get $message)) (i32.const 4))) (then
          (local.set $name (i32.trunc_f64_u (f64.load offset=16 (local.get $name))))
          (local.set $message (i32.trunc_f64_u (f64.load offset=16 (local.get $message))))
          (if (i32.eqz (i32.load offset=4 (local.get $message))) (then (return (local.get $name))))
          (if (i32.eqz (i32.load offset=4 (local.get $name))) (then (return (local.get $message))))
          (return (call $concat (call $concat (local.get $name) (i32.const {{: }})) (local.get $message)))))))))
    (i32.const {{[object Object]}}))

  (func $decimal (param $number i32) (result i32)
    (local $buffer i32)
    (local.set $buffer (call $realloc (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 40)))
    (call $lift (local.get $buffer) (call $number-format (f64.convert_i32_u (local.get $number)) (local.get $buffer))))

  (func (export "error.json") (param $result i64) (param $text i32) (result i32 f64)
    (local $code i32) (local $offset i32) (local $kind i32) (local $message i32)
    (local $data i32) (local $length i32) (local $index i32) (local $byte i32) (local $previous i32)
    (local $line i32) (local $column i32)
    (local.set $code (i32.wrap_i64 (i64.shr_u (local.get $result) (i64.const 32))))
    (local.set $offset (i32.wrap_i64 (local.get $result)))
    (local.set $kind (i32.const 4))
    (local.set $message (i32.const {{Invalid JSON}}))
    (if (i32.eq (local.get $code) (i32.const 2)) (then (local.set $message (i32.const {{Unpaired surrogate in JSON}}))))
    (if (i32.eq (local.get $code) (i32.const 3)) (then
      (local.set $kind (i32.const 2)) (local.set $message (i32.const {{JSON nesting limit exceeded}}))))
    (if (i32.eq (local.get $code) (i32.const 4)) (then
      (local.set $kind (i32.const 2)) (local.set $message (i32.const {{JSON storage limit exceeded}}))))
    (if (i32.eq (local.get $code) (i32.const 5)) (then
      (local.set $kind (i32.const 1)) (local.set $message (i32.const {{Invalid JSON value}}))))
    (if (i32.eq (local.get $code) (i32.const 6)) (then
      (local.set $kind (i32.const 0)) (local.set $message (i32.const {{Invalid JSON memory range}}))))
    (if (i32.eq (local.get $code) (i32.const 7)) (then
      (local.set $kind (i32.const 1)) (local.set $message (i32.const {{Invalid UTF-8 in JSON}}))))
    (if (i32.and (i32.ne (local.get $text) (i32.const 0)) (i32.le_u (local.get $code) (i32.const 3))) (then
      (local.set $data (i32.load (local.get $text)))
      (local.set $length (i32.load offset=4 (local.get $text)))
      (local.set $offset (select (local.get $offset) (local.get $length) (i32.lt_u (local.get $offset) (local.get $length))))
      (local.set $line (i32.const 1)) (local.set $column (i32.const 1))
      (block $done (loop $position
        (br_if $done (i32.ge_u (local.get $index) (local.get $offset)))
        (local.set $byte (i32.load8_u (i32.add (local.get $data) (local.get $index))))
        (if (i32.or (i32.eq (local.get $byte) (i32.const 10)) (i32.eq (local.get $byte) (i32.const 13)))
          (then
            (if (i32.or (i32.eq (local.get $byte) (i32.const 13)) (i32.ne (local.get $previous) (i32.const 13)))
              (then (local.set $line (i32.add (local.get $line) (i32.const 1)))))
            (local.set $column (i32.const 1)))
          (else
            (if (i32.or (i32.lt_u (local.get $byte) (i32.const 128)) (i32.ge_u (local.get $byte) (i32.const 192)))
              (then (local.set $column (i32.add (local.get $column)
                (select (i32.const 2) (i32.const 1) (i32.ge_u (local.get $byte) (i32.const 240)))))))))
        (local.set $previous (local.get $byte))
        (local.set $index (i32.add (local.get $index) (i32.const 1)))
        (br $position)))
      (local.set $message (call $concat (local.get $message) (i32.const {{ at byte }})))
      (local.set $message (call $concat (local.get $message) (call $decimal (local.get $offset))))
      (local.set $message (call $concat (local.get $message) (i32.const {{ (line }})))
      (local.set $message (call $concat (local.get $message) (call $decimal (local.get $line))))
      (local.set $message (call $concat (local.get $message) (i32.const {{, column }})))
      (local.set $message (call $concat (local.get $message) (call $decimal (local.get $column))))
      (local.set $message (call $concat (local.get $message) (i32.const {{)}})))))
    (i32.const 3) (f64.convert_i32_u (call $new (local.get $kind) (local.get $message) (i32.const 0))))

  (func $native (export "error.native") (param $metadata i32) (param $status i32) (param $payload f64) (param $context i32) (result f64)
    (local $kind i32) (local $name i32) (local $message i32) (local $error i32)
    (local $category i32) (local $context-key i32) (local $cause i32)
    (local $description i32) (local $entry i32) (local $end i32) (local $index f64)
    (if (i32.eq (local.get $status) (i32.const 3)) (then (return (local.get $payload))))
    (local.set $description (local.get $metadata))
    (local.set $entry (i32.load offset=12 (local.get $metadata)))
    (local.set $end (i32.add (local.get $entry) (i32.mul (i32.const 16) (i32.load offset=16 (local.get $metadata)))))
    (block $found (loop $cases
      (br_if $found (i32.eq (local.get $entry) (local.get $end)))
      (if (f64.eq (local.get $payload) (f64.convert_i32_u (i32.load (local.get $entry)))) (then
        (local.set $description (i32.add (local.get $entry) (i32.const 4))) (br $found)))
      (local.set $entry (i32.add (local.get $entry) (i32.const 16))) (br $cases)))
    (local.set $kind (i32.load (local.get $description)))
    (local.set $name (i32.load offset=4 (local.get $description)))
    (local.set $message (i32.load offset=8 (local.get $description)))
    (local.set $context-key (i32.load offset=32 (local.get $metadata)))
    (local.set $index (f64.sub (local.get $payload) (f64.convert_i32_u (i32.load offset=20 (local.get $metadata)))))
    (if (i32.and (i32.and (f64.ge (local.get $index) (f64.const 0))
        (f64.lt (local.get $index) (f64.convert_i32_u (i32.load offset=28 (local.get $metadata)))))
        (f64.eq (local.get $index) (f64.trunc (local.get $index)))) (then
      (local.set $category (i32.load (i32.add (i32.load offset=24 (local.get $metadata))
        (i32.mul (i32.const 4) (i32.trunc_f64_u (local.get $index))))))))
    (if (local.get $category) (then
      (local.set $message (call $concat (local.get $message) (i32.const {{: }})))
      (local.set $message (call $concat (local.get $message) (local.get $category)))))
    (if (local.get $context) (then
      (local.set $message (call $concat (local.get $message) (i32.const {{: }})))
      (local.set $message (call $concat (local.get $message) (local.get $context)))))
    (if (local.get $context-key) (then
      (if (i32.eqz (local.get $category)) (then
        (local.set $category (call $describe (call $box (i32.const 3) (local.get $payload))))))
      (local.set $cause (call $record (i32.const 2)))
      (call $set (local.get $cause) (i32.const {{code}}) (i32.const 4) (f64.convert_i32_u (local.get $category))) drop drop
      (if (local.get $context) (then
        (call $set (local.get $cause) (local.get $context-key) (i32.const 4) (f64.convert_i32_u (local.get $context))) drop drop))
      (local.set $cause (call $box (i32.const 6) (f64.convert_i32_u (local.get $cause))))))
    (local.set $error (call $new (local.get $kind) (local.get $message) (local.get $cause)))
    (call $hidden (i32.trunc_f64_u (f64.load offset=8 (local.get $error))) (i32.const {{name}}) (i32.const 4) (f64.convert_i32_u (local.get $name)))
    (call $hidden (i32.trunc_f64_u (f64.load offset=8 (local.get $error))) (i32.const {{code}}) (i32.const 3) (local.get $payload))
    (f64.convert_i32_u (local.get $error)))

  (func $normalize (export "error.normalize") (param $status i32) (param $payload f64) (result f64)
    (call $native (i32.const {{native-errors}}) (local.get $status) (local.get $payload) (i32.const 0)))

  (func (export "error.completion") (param $metadata i32) (param $status i32) (param $payload f64) (param $context i32) (result i32 f64)
    (if (i32.ne (local.get $status) (i32.const 1)) (then (return (local.get $status) (local.get $payload))))
    (i32.const 3) (call $native (local.get $metadata) (local.get $status) (local.get $payload) (local.get $context)))
)
