  ;; Operations: 0 stat, 1 mkdir, 2 unlink, 3 rmdir.
  (func (export "fs.metadata") (param $path i32) (param $operation i32) (param $valid i32) (result i32 f64)
    (local $frame i32) (local $scratch i32) (local $error i32) (local $directory i32)
    (local $data i32) (local $length i32) (local $result i32) (local $mtime f64)
    (if (i32.eqz (local.get $valid)) (then (return (i32.const 1) (f64.const 12))))
    (local.set $frame (call $frame-new (i32.const 4)))
    (i32.store offset=12 (local.get $frame) (local.get $path))
    (local.set $scratch (call $realloc (i32.const 0) (i32.const 0) (i32.const 8) (i32.const 112)))
    (i32.store offset=20 (local.get $frame) (local.get $scratch))
    (call $resolve-path (local.get $path) (local.get $scratch) (local.get $frame))
    local.set $length local.set $data local.set $directory local.set $error
    (if (i32.eqz (local.get $error)) (then
      (block $done
        (if (i32.eqz (local.get $operation)) (then
          (if (i32.eqz (local.get $length))
            (then (i32.store8 (local.get $data) (i32.const 46)) (local.set $length (i32.const 1))))
          (call $stat (local.get $directory) (i32.const 1) (local.get $data) (local.get $length) (local.get $scratch))
          (if (i32.load8_u (local.get $scratch))
            (then (local.set $error (i32.add (i32.load8_u offset=8 (local.get $scratch)) (i32.const 1))) (br $done)))
            ;; result<descriptor-stat, error>: payload at 8; size at 32;
            ;; modification option at 64, signed seconds at 72, nanos at 80.
            (local.set $result (call $realloc (i32.const 0) (i32.const 0) (i32.const 8) (i32.const 24)))
            (f64.store (local.get $result) (f64.convert_i64_u (i64.load offset=32 (local.get $scratch))))
            (if (i32.load8_u offset=64 (local.get $scratch)) (then
              (local.set $mtime (f64.add
                (f64.mul (f64.convert_i64_s (i64.load offset=72 (local.get $scratch))) (f64.const 1000))
                (f64.div (f64.convert_i32_u (i32.load offset=80 (local.get $scratch))) (f64.const 1000000))))))
            (f64.store offset=8 (local.get $result) (local.get $mtime))
            (i32.store offset=16 (local.get $result) (i32.load8_u offset=8 (local.get $scratch)))
          (br $done)))
        (if (i32.eqz (local.get $length)) (then
          (local.set $error (if (result i32) (i32.eq (local.get $operation) (i32.const 1))
            (then (i32.const 7)) (else (i32.const 31))))
          (br $done)))
        (if (i32.eq (local.get $operation) (i32.const 1))
          (then (call $mkdir (local.get $directory) (local.get $data) (local.get $length) (local.get $scratch)))
          (else (if (i32.eq (local.get $operation) (i32.const 2))
            (then (call $unlink (local.get $directory) (local.get $data) (local.get $length) (local.get $scratch)))
            (else (call $rmdir (local.get $directory) (local.get $data) (local.get $length) (local.get $scratch))))))
        (if (i32.load8_u (local.get $scratch))
          (then (local.set $error (i32.add (i32.load8_u offset=4 (local.get $scratch)) (i32.const 1))))))
      (call $drop-descriptor (local.get $directory))))
    (call $frame-drop (local.get $frame))
    (i32.ne (local.get $error) (i32.const 0))
    (f64.convert_i32_u (if (result i32) (local.get $error) (then (local.get $error)) (else (local.get $result)))))
