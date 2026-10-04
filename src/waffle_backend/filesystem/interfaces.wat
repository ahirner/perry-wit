  (import "wasi:filesystem/types@0.3.0" (instance $fs-types
    (export "descriptor" (type $descriptor (sub resource)))
    (type $borrow (borrow $descriptor))
    (type $own (own $descriptor))
    (type $error-base (variant
      (case "access") (case "already") (case "bad-descriptor") (case "busy")
      (case "deadlock") (case "quota") (case "exist") (case "file-too-large")
      (case "illegal-byte-sequence") (case "in-progress") (case "interrupted")
      (case "invalid") (case "io") (case "is-directory") (case "loop")
      (case "too-many-links") (case "message-size") (case "name-too-long")
      (case "no-device") (case "no-entry") (case "no-lock")
      (case "insufficient-memory") (case "insufficient-space") (case "not-directory")
      (case "not-empty") (case "not-recoverable") (case "unsupported") (case "no-tty")
      (case "no-such-device") (case "overflow") (case "not-permitted") (case "pipe")
      (case "read-only") (case "invalid-seek") (case "text-file-busy")
      (case "cross-device") (case "other" (option string))))
    (export "error-code" (type $error (eq $error-base)))
    (type $bytes (stream u8))
    (type $completion (future (result (error $error))))
    (type $path-flags-base (flags "symlink-follow"))
    (export "path-flags" (type $path-flags (eq $path-flags-base)))
    (type $open-flags-base (flags "create" "directory" "exclusive" "truncate"))
    (export "open-flags" (type $open-flags (eq $open-flags-base)))
    (type $descriptor-flags-base (flags "read" "write" "file-integrity-sync" "data-integrity-sync" "requested-write-sync" "mutate-directory"))
    (export "descriptor-flags" (type $descriptor-flags (eq $descriptor-flags-base)))
    (type $instant-base (record (field "seconds" s64) (field "nanoseconds" u32)))
    (export "instant" (type $instant (eq $instant-base)))
    (type $descriptor-type-base (variant
      (case "block-device") (case "character-device") (case "directory") (case "fifo")
      (case "symbolic-link") (case "regular-file") (case "socket") (case "other" (option string))))
    (export "descriptor-type" (type $descriptor-type (eq $descriptor-type-base)))
    (type $stat-base (record
      (field "type" $descriptor-type) (field "link-count" u64) (field "size" u64)
      (field "data-access-timestamp" (option $instant))
      (field "data-modification-timestamp" (option $instant))
      (field "status-change-timestamp" (option $instant))))
    (export "descriptor-stat" (type $stat (eq $stat-base)))
    (type $entry-base (record (field "type" $descriptor-type) (field "name" string)))
    (export "directory-entry" (type $entry (eq $entry-base)))
    (export "[method]descriptor.stat-at" (func async
      (param "self" $borrow) (param "path-flags" $path-flags) (param "path" string)
      (result (result $stat (error $error)))))
    (export "[method]descriptor.create-directory-at" (func async
      (param "self" $borrow) (param "path" string) (result (result (error $error)))))
    (export "[method]descriptor.unlink-file-at" (func async
      (param "self" $borrow) (param "path" string) (result (result (error $error)))))
    (export "[method]descriptor.remove-directory-at" (func async
      (param "self" $borrow) (param "path" string) (result (result (error $error)))))
    (export "[method]descriptor.read-directory" (func
      (param "self" $borrow) (result (tuple (stream $entry) $completion))))
    (export "[method]descriptor.open-at" (func async
      (param "self" $borrow) (param "path-flags" $path-flags) (param "path" string)
      (param "open-flags" $open-flags) (param "flags" $descriptor-flags)
      (result (result $own (error $error)))))
    (export "[method]descriptor.read-via-stream" (func
      (param "self" $borrow) (param "offset" u64) (result (tuple $bytes $completion))))
    (export "[method]descriptor.write-via-stream" (func
      (param "self" $borrow) (param "data" $bytes) (param "offset" u64) (result $completion)))))
  (alias export $fs-types "descriptor" (type $fs-descriptor))
  (alias export $fs-types "error-code" (type $fs-error))
  (alias export $fs-types "directory-entry" (type $fs-entry))
  (import "wasi:filesystem/preopens@0.3.0" (instance $fs-preopens
    (alias outer 1 $fs-descriptor (type $outer-descriptor))
    (export "descriptor" (type $descriptor (eq $outer-descriptor)))
    (export "get-directories" (func (result (list (tuple (own $descriptor) string)))))))
  (type $fs-completion (future (result (error $fs-error))))
  (type $fs-bytes (stream u8))
  (type $fs-entries (stream $fs-entry))
