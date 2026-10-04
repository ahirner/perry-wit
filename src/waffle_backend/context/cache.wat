  ;; Memory[76] caches a retained frame. Slots at 12 and 16 own argv and cwd.
  (func $cache (result i32) (local $cache i32)
    (local.set $cache (i32.load (i32.const 76)))
    (if (i32.eqz (local.get $cache)) (then
      (local.set $cache (call $retain (i32.const 2)))
      (i32.store (i32.const 76) (local.get $cache))))
    (local.get $cache))
