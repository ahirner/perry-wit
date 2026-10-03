(module
  (memory 1)
  ;; Program: EOI column offset, 256 byte classes, then fixed-width DFA rows.
  ;; A row starts with its match flag, followed by relative next-row offsets.
  (func (export "search") (param $text i32) (param $program i32) (result f64)
    (local $ptr i32) (local $len i32) (local $rows i32) (local $state i32)
    (local $cursor i32) (local $best i32) (local $class i32) (local $count i32)
    (local.set $ptr (i32.load (local.get $text)))
    (local.set $len (i32.load offset=4 (local.get $text)))
    (local.set $rows (i32.add (local.get $program) (i32.const 260)))
    (local.set $state (local.get $rows))
    (local.set $cursor (local.get $len))
    (local.set $best (i32.const -1))
    (block $scanned
      (loop $scan
        (br_if $scanned (i32.eqz (local.get $cursor)))
        (local.set $cursor (i32.sub (local.get $cursor) (i32.const 1)))
        (local.set $class (i32.load8_u offset=4 (i32.add (local.get $program)
          (i32.load8_u (i32.add (local.get $ptr) (local.get $cursor))))))
        (local.set $state (i32.add (local.get $rows)
          (i32.load offset=4 (i32.add (local.get $state) (i32.shl (local.get $class) (i32.const 2))))))
        (if (i32.load (local.get $state)) (then
          ;; DFA matches are delayed by one byte. Empty matches must not split UTF-8.
          (if (i32.eq (i32.add (local.get $cursor) (i32.const 1)) (local.get $len))
            (then (local.set $best (local.get $len)))
            (else
              (if (i32.ne
                (i32.and (i32.load8_u offset=1 (i32.add (local.get $ptr) (local.get $cursor))) (i32.const 192))
                (i32.const 128))
                (then (local.set $best (i32.add (local.get $cursor) (i32.const 1)))))))))
        (br $scan)))
    (local.set $state (i32.add (local.get $rows)
      (i32.load (i32.add (local.get $state) (i32.load (local.get $program))))))
    (if (i32.load (local.get $state)) (then (local.set $best (i32.const 0))))
    (if (i32.eq (local.get $best) (i32.const -1)) (then (return (f64.const -1))))
    (local.set $cursor (i32.const 0))
    (block $counted
      (loop $count
        (br_if $counted (i32.ge_u (local.get $cursor) (local.get $best)))
        (if (i32.ne (i32.and (i32.load8_u (i32.add (local.get $ptr) (local.get $cursor))) (i32.const 192)) (i32.const 128))
          (then (local.set $count (i32.add (local.get $count) (i32.const 1)))))
        (local.set $cursor (i32.add (local.get $cursor) (i32.const 1)))
        (br $count)))
    (f64.convert_i32_u (local.get $count))))
