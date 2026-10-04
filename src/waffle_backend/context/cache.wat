  ;; Memory[76] caches a retained frame. Slots at 12, 16, and 20 own argv, cwd, and env.
  (func $cache (result i32) (local $cache i32)
    (local.set $cache (i32.load (i32.const 76)))
    (if (i32.eqz (local.get $cache)) (then
      (local.set $cache (call $retain (i32.const 3)))
      (i32.store (i32.const 76) (local.get $cache))))
    (local.get $cache))
