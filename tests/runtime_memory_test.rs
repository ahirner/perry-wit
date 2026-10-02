use perry_wit::compiler::{CompileOptions, compile_typescript};
use std::{fs, process::Command};

#[test]
fn canonical_allocator_traps_when_allocating_or_growing_exhausts_memory() {
    let path = std::env::temp_dir().join(format!("perry-allocator-{}.wasm", std::process::id()));
    fs::write(
        &path,
        perry_wit::runtime::resolve_guest_runtime_bytes(None).unwrap(),
    )
    .unwrap();
    let output = Command::new("node").arg("--eval").arg(r#"
        const assert = require('node:assert/strict');
        const module = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        for (const grow of [false, true]) {
            const memory = new WebAssembly.Memory({initial: 32, maximum: 64});
            const imports = {env: {memory}};
            for (const {module: mod, name, kind} of WebAssembly.Module.imports(module)) {
                if (kind === 'function') {
                    (imports[mod] ??= {})[name] = () => { throw new Error(`unexpected host call ${mod} ${name}`); };
                }
            }
            const rt = new WebAssembly.Instance(module, imports).exports;
            let ptr = rt.cabi_realloc(0, 0, 16, 65536);
            assert.notEqual(ptr, 0);
            assert.equal(ptr % 16, 0);
            new Uint8Array(memory.buffer, ptr, 32).fill(123);
            let size = 65536;
            let successes = 0;
            assert.throws(() => {
                for (let i = 0; i < 256; i++) {
                    const next = grow ? rt.cabi_realloc(ptr, size, 16, size + 65536)
                                      : rt.cabi_realloc(0, 0, 16, 65536);
                    assert.notEqual(next, 0, 'allocation failure must trap, never return address zero');
                    if (grow) { ptr = next; size += 65536; }
                    assert.ok(new Uint8Array(memory.buffer, ptr, 32).every(byte => byte === 123));
                    successes++;
                }
                throw new Error('expected memory exhaustion');
            }, WebAssembly.RuntimeError);
            assert.ok(successes > 0);
            assert.ok(new Uint8Array(memory.buffer, ptr, 32).every(byte => byte === 123));
        }
    "#).arg(&path).output().unwrap();
    fs::remove_file(path).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn result_and_string_post_return_reclaim_payloads_and_exact_return_areas() {
    let scratch = std::env::temp_dir().join(format!("perry-memory-{}", std::process::id()));
    fs::create_dir_all(&scratch).unwrap();
    let compiled = compile_typescript(
        "",
        "memory.ts",
        &CompileOptions {
            core_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    fs::write(scratch.join("core.wasm"), compiled.core).unwrap();
    fs::write(scratch.join("check.cjs"), r#"
        const fs = require('node:fs');
        const assert = require('node:assert/strict');
        const module_ = new WebAssembly.Module(fs.readFileSync(process.argv[2]));
        const imports = {};
        for (const {module, name} of WebAssembly.Module.imports(module_)) {
            (imports[module] ??= {})[name] = () => { throw new Error(`unexpected WASI call ${module} ${name}`); };
        }
        const e = new WebAssembly.Instance(module_, imports).exports;
        e._start();
        const values = [true, false].map(ok => {
            const bytes = new TextEncoder().encode(JSON.stringify(ok ? {ok, value: '0'} : {ok, error: '0'}));
            const ptr = e.cabi_realloc(0, 0, 1, bytes.length);
            new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
            return e.cabi_import_json(ptr, bytes.length);
        });
        function cycle() {
            for (let branch = 0; branch < 2; branch++) {
                const ptr = e.cabi_export_result_string(values[branch]);
                const words = new Uint32Array(e.memory.buffer, ptr, 3);
                assert.equal(words[0], branch);
                assert.equal(new TextDecoder().decode(new Uint8Array(e.memory.buffer, words[1], words[2])), '0');
                e.cabi_post_result_cleanup(ptr);
            }
            e.cabi_post_cleanup(e.cabi_export_string(0n));
        }
        for (let i = 0; i < 1000; i++) cycle();
        const size = e.memory.buffer.byteLength;
        for (let i = 0; i < 100000; i++) cycle();
        assert.equal(e.memory.buffer.byteLength, size, 'post-return must keep linear memory bounded');
    "#).unwrap();
    let output = Command::new("node")
        .arg(scratch.join("check.cjs"))
        .arg(scratch.join("core.wasm"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(scratch).unwrap();
}
