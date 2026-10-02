use perry_wit::compiler::{CompileOptions, compile_typescript};
use std::{fs, process::Command};

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
        function cycle() {
            for (let branch = 0; branch < 2; branch++) {
                const ptr = e.cabi_export_result_string(0n, branch);
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
