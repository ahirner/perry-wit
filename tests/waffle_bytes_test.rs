#[path = "support/waffle.rs"]
mod waffle_fixture;
use std::{fs, process::Command};
use waffle_fixture::compile_typescript_waffle;

use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimitsBuilder};

#[tokio::test(flavor = "current_thread")]
async fn byte_views_preserve_aliases_copies_and_lengths() -> Result<()> {
    let source = r#"
    function make(): Uint8Array { return new Uint8Array([0, 255, 128, 17]); }
    function mutate(view: Uint8Array): number { view[0] = 99; return view.length; }
    export function run(): number {
        const source = make();
        const middle = source.subarray(1, -1);
        const tail = middle.subarray(-1);
        const copy = new Uint8Array(middle);
        const sliced = middle.slice(0, 1);
        const alias = middle;
        mutate(tail);
        if (alias !== middle) { return -1; }
        if (copy === middle) { return -1; }
        if (sliced === middle) { return -1; }
        if (source[2] !== 99) { return -2; }
        if (tail[0] !== 99) { return -2; }
        if (middle[1] !== 99) { return -2; }
        if (copy[1] !== 128) { return -3; }
        if (sliced[0] !== 255) { return -3; }
        return source.length * 1000 + middle.byteLength * 100 + tail.byteOffset * 10 + tail.length;
    }"#;
    assert_eq!(run(source, &[]).await?, vec![4221.0; 3]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn backing_buffers_preserve_identity_and_reclaim_dead_aliases() -> Result<()> {
    let source = r#"export function run(): number {
        const bytes = new Uint8Array([3, 7, 11]);
        const buffer = bytes.buffer;
        const alias = new Uint8Array(buffer);
        if (alias.buffer !== buffer || buffer.byteLength !== 3) return -1;
        for (let index = 0; index < 10000; index++) {
            const view = alias.subarray(1, 2);
            if (view.buffer !== buffer || view.byteOffset !== 1 || view.byteLength !== 1) return -2;
            view[0] = index;
        }
        if (new Uint8Array(bytes).buffer === buffer) return -3;
        return bytes[1];
    }"#;
    assert_eq!(run(source, &[]).await?, vec![15.0; 3]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byte_conversions_and_out_of_bounds_access_are_explicit() -> Result<()> {
    let source = r#"
    export function run(value: number): number {
        const bytes = new Uint8Array(2.9);
        const assigned = (bytes[0] = value);
        bytes[1] = -257.9;
        bytes[-1] = 91;
        bytes[0.5] = 92;
        bytes[2] = 93;
        if (bytes[-1] !== undefined) { return -1; }
        if (bytes[0.5] !== undefined) { return -1; }
        if (bytes[2] !== undefined) { return -1; }
        if (bytes[1] !== 255) { return -2; }
        if (bytes.length !== 2) { return -2; }
        return bytes[0];
    }"#;
    assert_eq!(
        run(source, &[0.0, -1.0, 257.9, f64::NAN, f64::INFINITY, 1e300]).await?,
        vec![0.0, 255.0, 1.0, 0.0, 0.0, 0.0]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byte_length_errors_unwind_and_instances_remain_reusable() -> Result<()> {
    let source = r#"
    export function run(value: number): number {
        try {
            const bytes = new Uint8Array(value);
            return bytes.length;
        } catch { return -1; }
    }"#;
    assert_eq!(
        run(source, &[-1.0, f64::INFINITY, f64::NAN, -0.5, 17.9]).await?,
        vec![-1.0, -1.0, 0.0, 0.0, 17.0]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retained_subviews_keep_backing_bytes_alive_during_collection() -> Result<()> {
    let source = r#"
    function retained(): Uint8Array {
        const owner = new Uint8Array([3, 7, 11]);
        return owner.subarray(1, 2);
    }
    export function run(): number {
        const live = retained();
        let index = 0;
        while (index < 10000) {
            const scratch = new Uint8Array([index, 255]);
            const transient = scratch.subarray(1);
            if (transient[0] !== 255) { return -1; }
            if (live[0] !== 7) { return -1; }
            index = index + 1;
        }
        return live[0];
    }"#;
    assert_eq!(run(source, &[]).await?, vec![7.0; 3]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn allocation_free_byte_loops_have_a_bounded_instruction_cost() -> Result<()> {
    let compiled = compile_typescript_waffle(
        r#"export function run(): number {
            let total = 0;
            for (let round = 0; round < 4; round++) {
                const bytes = new Uint8Array([7]);
                for (let index = 0; index < 20000; index++) total += bytes[0];
            }
            return total;
        }"#,
        "byte-loop-cost.ts",
        &WaffleCompileOptions::default(),
    )?;
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65536).build(),
    );
    store.limiter(|limits| limits);
    store.set_fuel(10_000_000)?;
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        store.set_fuel(10_000_000)?;
        assert_eq!(run.call_async(&mut store, ()).await?.0, 560000.0);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn subarray_bounds_and_argument_effects_match_node() -> Result<()> {
    let source = r#"
    function start(owner: Uint8Array): number { owner[0] = 20; return -3; }
    function end(owner: Uint8Array): number { owner[0] = owner[0] + 1; return -1; }
    export function run(value: number): number {
        const bytes = new Uint8Array([10, 20, 30, 40]);
        const ignored = bytes.subarray(start(bytes), end(bytes));
        let missing = undefined;
        const tail = bytes.subarray(value, missing);
        const reversed = bytes.subarray(3, 1);
        if (bytes[0] !== 21) { return -1; }
        if (ignored.length !== 2) { return -2; }
        if (reversed.length !== 0) { return -3; }
        return tail.length * 100 + tail.byteOffset;
    }"#;
    assert_eq!(
        run(
            source,
            &[
                -99.0,
                -2.9,
                -0.5,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY
            ]
        )
        .await?,
        vec![400.0, 202.0, 400.0, 400.0, 4.0, 400.0]
    );
    Ok(())
}

#[test]
fn unsupported_byte_forms_fail_before_emission() {
    for source in [
        "export function run(): number { return new Uint8Array(2); }",
        "export function run(): boolean { const a = new Uint8Array(1); return a < a; }",
        "export function run(): number { const a = new Uint8Array(2, 1); return a.length; }",
        "export function run(): number { const a = new Uint8Array('3'); return a.length; }",
        "export function run(): number { const a = new Uint8Array(3); return a.subarray(0, 1, 2).length; }",
    ] {
        assert!(
            compile_typescript_waffle(
                source,
                "unsupported-bytes.ts",
                &WaffleCompileOptions::default()
            )
            .is_err(),
            "{source}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn canonical_bytes_round_trip_arbitrary_data_and_reclaim_all_outcomes() -> Result<()> {
    let source = r#"
    export async function run(input: Uint8Array, fail: boolean): Promise<Result<Uint8Array, number>> {
        const copy = new Uint8Array(input);
        copy[0] = 99;
        if (fail) { throw 17; }
        return copy.subarray(1, -1);
    }"#;
    let compiled =
        compile_typescript_waffle(source, "byte-boundary.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    assert_eq!(component.component_type().imports(&engine).count(), 0);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65536).build(),
    );
    store.limiter(|limits| limits);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let run = instance.get_typed_func::<(Vec<u8>, bool), (std::result::Result<Vec<u8>, f64>,)>(
        &mut store, "run",
    )?;
    for size in [0usize, 1, 2, 8193] {
        let input: Vec<u8> = (0..size)
            .map(|index| (index * 37 + index / 251) as u8)
            .collect();
        for cycle in 0..30 {
            let fail = cycle % 3 == 1;
            let result = run.call_async(&mut store, (input.clone(), fail)).await?.0;
            let expected = if fail {
                Err(17.0)
            } else if input.len() <= 2 {
                Ok(vec![])
            } else {
                Ok(input[1..input.len() - 1].to_vec())
            };
            assert_eq!(result, expected);
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn direct_byte_results_and_mixed_text_parameters_use_distinct_descriptors() -> Result<()> {
    let source = r#"
    export function run(text: string, input: Uint8Array): Uint8Array {
        input[0] = text.length;
        return input;
    }"#;
    let compiled =
        compile_typescript_waffle(source, "mixed-bytes.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let run = instance.get_typed_func::<(String, Vec<u8>), (Vec<u8>,)>(&mut store, "run")?;
    for _ in 0..5 {
        assert_eq!(
            run.call_async(&mut store, ("😀é".into(), vec![255, 128, 0]))
                .await?
                .0,
            [2, 128, 0]
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byte_entry_outcomes_survive_stored_primitive_tasks() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    for result_type in ["Uint8Array", "Result<Uint8Array, number>"] {
        let source = format!(
            r#"
        async function work(): Promise<number> {{ await 0; return 7; }}
        export async function run(input: Uint8Array): Promise<{result_type}> {{
            const view = input.subarray(1);
            const pending = work();
            view[0] = await pending;
            return view;
        }}"#
        );
        let compiled = compile_typescript_waffle(
            &source,
            "async-byte-boundary.ts",
            &WaffleCompileOptions::default(),
        )?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(65536).build(),
        );
        store.limiter(|limits| limits);
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        for _ in 0..30 {
            let input = vec![255, 128, 0, 254];
            let result = if result_type == "Uint8Array" {
                let run = instance.get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "run")?;
                run.call_async(&mut store, (input,)).await?.0
            } else {
                let run = instance
                    .get_typed_func::<(Vec<u8>,), (std::result::Result<Vec<u8>, f64>,)>(
                        &mut store, "run",
                    )?;
                run.call_async(&mut store, (input,)).await?.0.unwrap()
            };
            assert_eq!(result, [7, 0, 254]);
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

async fn run(source: &str, inputs: &[f64]) -> Result<Vec<f64>> {
    let compiled = compile_typescript_waffle(source, "bytes.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    assert_eq!(component.component_type().imports(&engine).count(), 0);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65536).build(),
    );
    store.limiter(|limits| limits);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let mut results = Vec::new();
    if inputs.is_empty() {
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        for _ in 0..3 {
            results.push(run.call_async(&mut store, ()).await?.0);
        }
    } else {
        let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
        for &input in inputs {
            results.push(run.call_async(&mut store, (input,)).await?.0);
        }
    }
    store.assert_concurrent_state_empty();
    let arguments = inputs
        .iter()
        .map(|value| {
            if value.is_nan() {
                "NaN".into()
            } else if *value == f64::INFINITY {
                "Infinity".into()
            } else if *value == f64::NEG_INFINITY {
                "-Infinity".into()
            } else {
                value.to_string()
            }
        })
        .collect::<Vec<_>>();
    let calls = if arguments.is_empty() {
        "[run(), run(), run()]".into()
    } else {
        format!("[{}].map(value => run(value))", arguments.join(","))
    };
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bytes.mts");
    fs::write(
        &path,
        format!("{source}\nconsole.log(JSON.stringify({calls}));"),
    )?;
    let node = Command::new("node").arg(path).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(results, serde_json::from_slice::<Vec<f64>>(&node.stdout)?);
    Ok(results)
}

#[tokio::test(flavor = "current_thread")]
async fn typed_array_set_preserves_overlap_ranges_offsets_and_failure_atomicity() -> Result<()> {
    let source = r#"
    export function run(offset:number):number {
      const bytes=new Uint8Array([1,2,3,4,5]);
      bytes.set(bytes.subarray(0,4),1);
      if(bytes[0]!==1||bytes[1]!==1||bytes[2]!==2||bytes[3]!==3||bytes[4]!==4)throw 1;
      bytes.set(bytes.subarray(1),0);
      if(bytes[0]!==1||bytes[1]!==2||bytes[2]!==3||bytes[3]!==4||bytes[4]!==4)throw 2;
      const target=bytes.subarray(1,4);
      try {target.set(new Uint8Array([7,8]),offset);}
      catch {return bytes[0]*10000+bytes[1]*1000+bytes[2]*100+bytes[3]*10+bytes[4];}
      return bytes[0]*10000+bytes[1]*1000+bytes[2]*100+bytes[3]*10+bytes[4];
    }"#;
    let inputs = [
        f64::NAN,
        -0.9,
        1.9,
        2.,
        -1.,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1e20,
    ];
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("set.mts");
    fs::write(
        &script,
        format!(
            "{source}\nconsole.log(JSON.stringify([NaN,-0.9,1.9,2,-1,Infinity,-Infinity,1e20].map(run)));"
        ),
    )?;
    let node = Command::new("node").arg(script).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    let expected: Vec<f64> = serde_json::from_slice(&node.stdout)?;
    assert_eq!(run(source, &inputs).await?, expected);
    Ok(())
}
