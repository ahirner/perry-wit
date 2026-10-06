use anyhow::Result;
use perry_wit::waffle_backend::{
    WaffleCompileOptions, WaffleCompiled, compile_typescript_for_world,
};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};

#[path = "support/heap_measurement.rs"]
mod heap_measurement;

mod records {
    wasmtime::component::bindgen!({path: "tests/fixtures/record-boundary", world: "boundary"});
}

fn compile_records(source: &str) -> Result<WaffleCompiled> {
    compile_world(source, include_str!("fixtures/record-boundary/world.wit"))
}

#[test]
fn literal_projections_avoid_materialization() -> Result<()> {
    let engine = Engine::default();
    for (kind, bodies, helpers) in [
        (
            "dense array",
            [
                "return [x, x + 1, x + 2][1];",
                "const values:number[] = [x, x + 1, x + 2]; return values[1];",
            ],
            "",
        ),
        (
            "Date timestamp",
            [
                "return new Date(x + 1).getTime();",
                "const value = new Date(x + 1); return value.getTime();",
            ],
            "",
        ),
        (
            "byte array",
            [
                "return new Uint8Array([x, x + 1, x + 2])[1] + 0;",
                "const values = new Uint8Array([x, x + 1, x + 2]); return values[1] + 0;",
            ],
            "",
        ),
        (
            "JSON string field",
            [
                "return identity(JSON.parse(JSON.stringify({text:'abc'})).text.length) + x - 2;",
                "const text=JSON.stringify({text:'abc'}); return identity(JSON.parse(text).text.length) + x - 2;",
            ],
            "function identity(value:number):number {return value;}\n",
        ),
    ] {
        let mut measurements = Vec::new();
        for body in bodies {
            let compiled = compile_world(
                &format!("{helpers}export function run(x:number):number {{{body}}}"),
                "package test:boundary; world boundary {export run:func(x:f64)->f64;}",
            )?;
            let bytes = compiled.component.as_ref().unwrap().len();
            let core = heap_measurement::instrument(&compiled.core)?;
            let mut resolve = wit_parser::Resolve::default();
            let package = resolve.push_str(
                "probe.wit",
                "package test:boundary; world boundary {
            export run:func(x:f64)->f64;
            export measure-allocations:func()->u64;
        }",
            )?;
            let world = resolve.select_world(&[package], Some("boundary"))?;
            let component = Component::new(
                &engine,
                perry_wit::waffle_backend::encode_component(&core, resolve, world)?,
            )?;
            let mut store = Store::new(&engine, ());
            let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
            let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
            let allocations =
                instance.get_typed_func::<(), (u64,)>(&mut store, "measure-allocations")?;
            let before = allocations.call(&mut store, ())?.0;
            for input in 0..16 {
                assert_eq!(
                    run.call(&mut store, (f64::from(input),))?.0,
                    f64::from(input + 1)
                );
            }
            measurements.push((bytes, allocations.call(&mut store, ())?.0 - before));
        }
        assert!(
            measurements[0].0 < measurements[1].0,
            "component sizes: {measurements:?}"
        );
        assert!(
            measurements[0].1 < measurements[1].1,
            "allocation counts: {measurements:?}"
        );
        println!(
            "{kind} projection versus materialization (component bytes, allocations for 16 calls): {measurements:?}"
        );
    }
    Ok(())
}

#[test]
fn immediate_json_string_projection_preserves_effects_duplicates_and_live_values() -> Result<()> {
    let source = r#"
        function text(trace:number[], value:number):string {
            trace.push(value); return 'p' + (value === 7 ? 'last' : 'a');
        }
        function fail():string {throw 9;}
        function churn():string {
            for(let i=0;i<1000;i++){const garbage:string[]=['a','b'];}
            return '';
        }
        export function run():number[] {
            const trace:number[]=[];
            const selected=JSON.parse(JSON.stringify({first:text(trace,1),selected:text(trace,2),last:text(trace,3)})).selected;
            trace.push(selected.length);
            try {const value=JSON.parse(JSON.stringify({keep:text(trace,4),other:fail(),last:text(trace,5)})).keep;trace.push(value.length);}
            catch(error){trace.push(error as number);}
            const duplicate=JSON.parse(JSON.stringify({text:text(trace,6),text:text(trace,7)})).text;
            trace.push(duplicate.length);
            const kept=JSON.parse(JSON.stringify({text:text(trace,8),unused:churn()})).text;
            trace.push(kept.length);
            return trace;
        }
    "#;
    let compiled = compile_world(
        source,
        "package test:boundary; world boundary {export run:func()->list<f64>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new()
            .memory_size(8 * 1024 * 1024)
            .build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(), (Vec<f64>,)>(&mut store, "run")?;
    for _ in 0..16 {
        assert_eq!(
            run.call(&mut store, ())?.0,
            [1., 2., 3., 2., 4., 9., 6., 7., 5., 8., 2.]
        );
    }
    Ok(())
}

#[test]
fn immediate_json_string_projection_preserves_string_boundaries() -> Result<()> {
    let compiled = compile_world(
        "export function run(value:string):string {return JSON.parse(JSON.stringify({text:value})).text;}",
        "package test:boundary; world boundary {export run:func(value:string)->string;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    for input in ["", "a\0b", "é漢", "e\u{301}", "😀", "\"\\\n"] {
        assert_eq!(run.call(&mut store, (input.into(),))?.0, input);
    }
    Ok(())
}

#[test]
fn immediate_date_timestamp_preserves_clipping_effects_and_exceptions() -> Result<()> {
    let source = r#"
        function record(trace:number[], value:number):number {trace.push(value);return value;}
        function fail():number {throw 9;}
        function text(trace:number[]):string {trace.push(7);return '1970-01-01T00:00:00.000Z';}
        export function run(x:number):number[] {
            const trace:number[]=[];
            trace.push(new Date(record(trace,x)).getTime());
            const stored=new Date(x);
            trace.push(stored.getTime());
            try {const value=new Date(fail()).getTime();trace.push(value);}
            catch(error){trace.push(error as number);}
            trace.push(new Date(text(trace)).getTime());
            trace.push(new Date('invalid').getTime());
            return trace;
        }
    "#;
    let compiled = compile_world(
        source,
        "package test:boundary; world boundary {export run:func(x:f64)->list<f64>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(f64,), (Vec<f64>,)>(&mut store, "run")?;
    for (input, expected) in [
        (-0.0, 0.0),
        (-0.5, 0.0),
        (0.5, 0.0),
        (-1.9, -1.0),
        (8_640_000_000_000_000.0, 8_640_000_000_000_000.0),
        (-8_640_000_000_000_000.0, -8_640_000_000_000_000.0),
        (8_640_000_000_000_001.0, f64::NAN),
        (-8_640_000_000_000_001.0, f64::NAN),
        (f64::NAN, f64::NAN),
        (f64::INFINITY, f64::NAN),
        (f64::NEG_INFINITY, f64::NAN),
    ] {
        let actual = run.call(&mut store, (input,))?.0;
        let expected = [input, expected, expected, 9.0, 7.0, 0.0, f64::NAN];
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.into_iter().zip(expected) {
            if expected.is_nan() {
                assert!(actual.is_nan());
            } else {
                assert_eq!(actual.to_bits(), expected.to_bits(), "input {input:?}");
            }
        }
    }
    Ok(())
}

#[test]
fn literal_byte_projection_preserves_conversion_effects_and_exception_order() -> Result<()> {
    let source = r#"
        function record(trace:number[], value:number):number {trace.push(value);return value;}
        function fail():number {throw 9;}
        export function run():number[] {
            const trace:number[]=[];
            const selected=new Uint8Array([record(trace,-257.9),record(trace,2),record(trace,3)])[0];
            trace.push(selected);
            try {const value=new Uint8Array([record(trace,4),fail(),record(trace,5)])[0]; trace.push(value);}
            catch(error){trace.push(error);}
            const truth = new Uint8Array([true,false,undefined])[0];
            const missing = new Uint8Array([true,false,undefined])[2];
            trace.push(truth); trace.push(missing);
            return trace;
        }
    "#;
    let compiled = compile_world(
        source,
        "package test:boundary; world boundary {export run:func()->list<f64>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(), (Vec<f64>,)>(&mut store, "run")?;
    assert_eq!(
        run.call(&mut store, ())?.0,
        [-257.9, 2.0, 3.0, 255.0, 4.0, 9.0, 1.0, 0.0]
    );
    Ok(())
}

#[test]
fn literal_array_projection_preserves_effects_exceptions_and_aliases() -> Result<()> {
    let source = r#"
        function record(trace:number[], value:number):number {trace.push(value);return value;}
        function fail():number {throw 9;}
        function churn():number {for(let i=0;i<1000;i++){const garbage:number[]=[i,i+1];}return 0;}
        export function run():number[] {
            const trace:number[]=[];
            const selected=[record(trace,1),record(trace,2),record(trace,3)][1];
            trace.push(selected);
            try {const value=[record(trace,4),fail(),record(trace,5)][0]; trace.push(value);}
            catch(error){trace.push(error);}
            const held=[trace,churn()][0];
            held.push(6);
            return trace;
        }
    "#;
    let compiled = compile_world(
        source,
        "package test:boundary; world boundary {export run:func()->list<f64>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(), (Vec<f64>,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call(&mut store, ())?.0, [1., 2., 3., 2., 4., 9., 6.]);
    }
    Ok(())
}

#[test]
fn conditional_tagged_values_are_checked_only_on_the_selected_branch() -> Result<()> {
    let engine = Engine::default();
    for (body, selected) in [
        ("return {ok:true,value:select ? data.value : 7};", true),
        ("return {ok:true,value:select ? 7 : data.value};", false),
        (
            "let value=7; if(select){value=data.value;} return {ok:true,value:value};",
            true,
        ),
        (
            "let value=7; for(let i=0;i<2;i++){if(select){value=data.value;}} return {ok:true,value:value};",
            true,
        ),
    ] {
        let source = format!(
            r#"
            export function run(input:string, select:boolean):{{ok:true,value:number}}|{{ok:false,error:number}} {{
                const data = JSON.parse(input);
                try {{ {body} }}
                catch(error) {{ return {{ok:false,error:error}}; }}
            }}
        "#
        );
        let compiled = compile_world(
            &source,
            "package test:boundary; world boundary {export run:func(input:string,select:bool)->result<f64,f64>;}",
        )?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let run =
            instance.get_typed_func::<(&str, bool), (Result<f64, f64>,)>(&mut store, "run")?;
        assert_eq!(
            run.call(&mut store, (r#"{"value":42}"#, selected))?.0,
            Ok(42.0)
        );
        assert_eq!(
            run.call(&mut store, (r#"{"value":"wrong"}"#, selected))?.0,
            Err(12.0)
        );
        assert_eq!(
            run.call(&mut store, (r#"{"value":"wrong"}"#, !selected))?.0,
            Ok(7.0)
        );
    }
    compile_world(
        "export function run(select:boolean):number {return select ? 'wrong' : 7;}",
        "package test:boundary; world boundary {export run:func(select:bool)->f64;}",
    )
    .expect_err("incompatible concrete branch types remain rejected");
    Ok(())
}

#[test]
fn enum_contracts_reject_nonmember_literals() {
    let wit = "package test:boundary; world boundary {
        enum answer { yes, no }
        export run:func()->answer;
    }";
    for source in [
        "export function run():'maybe' {return 'maybe';}",
        "export function run():'yes'|'no' {return 'maybe';}",
        "export function run():string {return 'yes';}",
    ] {
        assert!(compile_world(source, wit).is_err(), "{source}");
    }
    compile_world("export function run():'yes'|'no' {return 'yes';}", wit)
        .expect("a member of the declared enum is valid");
}

#[test]
fn generated_sdk_exposes_plain_date_and_weekday_properties() -> Result<()> {
    check_sdk_source(
        "package test:boundary; world boundary {export run:func()->string;}",
        "export function run():string {
            const date:Temporal.PlainDate=Temporal.PlainDate.from('2024-01-01');
            const datetime=Temporal.PlainDateTime.from('2024-01-01T12:00');
            return date.add({days:date.dayOfWeek+datetime.dayOfWeek}).toString();
        }",
    )
}

#[test]
fn resolved_guest_calls_extract_dynamic_json_arguments_and_reject_wrong_tags() -> Result<()> {
    let source = r#"
    function increment(value:number):number {return value+1;}
    function length(value:string):number {return value.length;}
    function field(value:{x:number}):number {return value.x;}
    export function run(input:string):{ok:true,value:number}|{ok:false,error:number} {
        const value=JSON.parse(input);
        try {return {ok:true,value:increment(value.n)+length(value.s)+field(value.obj)};}
        catch(error) {return {ok:false,error:error};}
    }"#;
    let compiled = compile_world(
        source,
        "package test:boundary; world boundary {export run:func(input:string)->result<f64,f64>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&str,), (Result<f64, f64>,)>(&mut store, "run")?;
    for (input, expected) in [
        (r#"{"n":42,"s":"abc","obj":{"x":7}}"#, Ok(53.0)),
        (r#"{"n":"42","s":"abc","obj":{"x":7}}"#, Err(12.0)),
        (r#"{"n":42,"s":3,"obj":{"x":7}}"#, Err(12.0)),
        (r#"{"n":42,"s":"abc","obj":7}"#, Err(12.0)),
    ] {
        assert_eq!(run.call(&mut store, (input,))?.0, expected);
    }
    Ok(())
}

#[test]
fn record_tuple_and_result_exports_remain_valid_under_bounded_memory() -> Result<()> {
    use records::exports::test::records::transform::{Failure, Input};
    let compiled = compile_records(include_str!("fixtures/record_boundary.ts"))?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = records::Boundary::instantiate(&mut store, &component, &Linker::new(&engine))?;
    let transform = instance.test_records_transform();
    for _ in 0..300 {
        for (label, samples, adjust, expected) in [
            ("漢字🙂", (4, 5, 6), -2, Ok(("漢字🙂!".into(), 13))),
            ("zero", (-7, 3, 4), 0, Ok(("zero!".into(), 0))),
            ("", (1, 2, 3), 0, Err(Failure::EmptyLabel)),
            ("negative", (-9, 1, 2), 0, Err(Failure::Negative)),
        ] {
            assert_eq!(
                transform.call_summarize(
                    &mut store,
                    &Input {
                        label: label.into(),
                        samples,
                        adjust
                    }
                )?,
                expected
            );
        }
    }
    Ok(())
}

#[test]
fn record_source_matches_the_generated_export_sdk() -> Result<()> {
    check_sdk_source(
        include_str!("fixtures/record-boundary/world.wit"),
        include_str!("fixtures/record_boundary.ts"),
    )
}

#[path = "support/wit_source.rs"]
mod wit_source;
use wit_source::check_sdk_source;

fn compile_world(source: &str, wit: &str) -> Result<WaffleCompiled> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("boundary.wit", wit)?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    compile_typescript_for_world(
        source,
        "boundary.ts",
        &WaffleCompileOptions::default(),
        resolve,
        world,
    )
}

#[test]
fn resolved_exports_handle_indirect_records_tuples_strings_and_enum_identity() -> Result<()> {
    const WIT: &str = "package test:boundary; world boundary {
        enum mode { first, second }
        record packet { code: s16, label: string, values: tuple<u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32> }
        export echo: func(request: packet) -> packet;
        export select: func(value: mode) -> mode;
    }";
    let source = r#"
        type Mode = "first" | "second";
        interface Packet { code:number; label:string; values:[number,number,number,number,number,number,number,number,number,number,number,number,number,number,number,number,number,number]; }
        export function echo(request:Packet):Packet {let index=0; while(index<600){const discarded=request.label+'x';index=index+1;} return request;}
        export function select(value:Mode):Mode {return value;}
    "#;
    let compiled = compile_world(source, WIT)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let echo = instance.get_func(&mut store, "echo").unwrap();
    let select = instance.get_func(&mut store, "select").unwrap();
    use wasmtime::component::Val;
    let packet = Val::Record(vec![
        ("code".into(), Val::S16(-32768)),
        ("label".into(), Val::String("漢字🙂".repeat(100))),
        (
            "values".into(),
            Val::Tuple((0..18).map(|i| Val::U32(u32::MAX - i)).collect()),
        ),
    ]);
    for _ in 0..20 {
        let mut results = [Val::Bool(false)];
        echo.call(&mut store, std::slice::from_ref(&packet), &mut results)?;
        assert_eq!(results[0], packet);
        for case in ["first", "second"] {
            select.call(&mut store, &[Val::Enum(case.into())], &mut results)?;
            assert_eq!(results[0], Val::Enum(case.into()));
        }
    }
    Ok(())
}

#[test]
fn integer_exports_reject_fractional_out_of_range_and_nonfinite_values() -> Result<()> {
    let compiled = compile_world(
        "export function checked(value:number):number {return value;}",
        "package test:boundary; world boundary {export checked: func(value: f64) -> u8;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    for (value, expected) in [
        (0.0, Some(0)),
        (255.0, Some(255)),
        (-1.0, None),
        (256.0, None),
        (1.5, None),
        (f64::NAN, None),
        (f64::INFINITY, None),
    ] {
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let checked = instance.get_typed_func::<(f64,), (u8,)>(&mut store, "checked")?;
        match expected {
            Some(expected) => {
                assert_eq!(checked.call(&mut store, (value,))?.0, expected);
            }
            None => assert!(checked.call(&mut store, (value,)).is_err(), "{value}"),
        }
    }
    Ok(())
}

#[test]
fn resolved_wit_rejects_incompatible_source_contracts() {
    let source = include_str!("fixtures/record_boundary.ts");
    for (from, to) in [
        ("transformSummarize", "wrongName"),
        ("adjust: number", "adjust: string"),
        (
            "samples: [number, number, number]",
            "samples: [number, number]",
        ),
        ("\"empty-label\" | \"negative\"", "string"),
        ("input.samples[index]", "input.samples[3]"),
        ("input.samples[index]", "input.samples['0']"),
        ("input.samples[index]", "(input as any).missing[index]"),
        (
            "let total = input.adjust;",
            "input.adjust='wrong';let total = 0;",
        ),
        (
            "{ ok: false, error: \"empty-label\" }",
            "{ ok: true, error: 'empty-label' }",
        ),
        (
            "{ ok: false, error: \"empty-label\" }",
            "{ ok: false, error: 1 }",
        ),
        (
            "{ ok: false, error: \"empty-label\" }",
            "{ ok: false, error: 'wrong-error' }",
        ),
        ("[input.label + \"!\", total]", "[total, total]"),
        ("[input.label + \"!\", total]", "[input.label]"),
    ] {
        assert!(source.contains(from), "missing replacement: {from}");
        assert!(
            compile_records(&source.replace(from, to)).is_err(),
            "accepted {from} -> {to}"
        );
    }
}

#[test]
fn tuple_bounds_throw_through_typed_result_without_growing_the_array() -> Result<()> {
    let source = r#"
        type Outcome = {ok:true;value:number} | {ok:false;error:number};
        export function at(index:number):Outcome {
            let values:[number,number]=[1,2];
            values=[7,9];
            try {return {ok:true,value:values[index]};}
            catch(error) {return {ok:false,error:12};}
        }
    "#;
    let compiled = compile_world(
        source,
        "package test:boundary; world boundary {export at:func(index:f64)->result<u8,u8>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let at = instance.get_typed_func::<(f64,), (Result<u8, u8>,)>(&mut store, "at")?;
    for index in [-1.0, 0.0, 1.0, 2.0, 0.5, f64::NAN, f64::INFINITY] {
        let expected = if index == 0.0 {
            Ok(7)
        } else if index == 1.0 {
            Ok(9)
        } else {
            Err(12)
        };
        assert_eq!(at.call(&mut store, (index,))?.0, expected);
    }
    for replacement in [
        "values=[7]",
        "values=[7,'x']",
        "values[index]=7",
        "values.length=3",
        "(values as any)[index]=7",
    ] {
        assert!(
            compile_world(
                &source.replace("values=[7,9]", replacement),
                "package test:boundary; world boundary {export at:func(index:f64)->result<u8,u8>;}"
            )
            .is_err(),
            "{replacement}"
        );
    }
    Ok(())
}

#[test]
fn empty_result_payloads_preserve_the_discriminant() -> Result<()> {
    let compiled = compile_world(
        "type Outcome={ok:true}|{ok:false}; export function check(value:boolean):Outcome {return {ok:value};}",
        "package test:boundary; world boundary {export check:func(value:bool)->result;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let check = instance.get_typed_func::<(bool,), (Result<(), ()>,)>(&mut store, "check")?;
    assert_eq!(check.call(&mut store, (true,))?.0, Ok(()));
    assert_eq!(check.call(&mut store, (false,))?.0, Err(()));
    for invalid in ["{ok:false}", "{ok:true,error:'unexpected'}"] {
        let source = format!(
            "type Outcome={{ok:true}}|{{ok:false;error:string}}; export function check(value:boolean):Outcome {{return {invalid};}}"
        );
        assert!(compile_world(&source,"package test:boundary; world boundary {export check:func(value:bool)->result<_,string>;}").is_err(),"{invalid}");
    }
    Ok(())
}

#[test]
fn resolved_wit_rejects_any_in_helpers_aliases_and_casts() {
    let wit = "package test:boundary; world boundary {export check:func(value:f64)->f64;}";
    for source in [
        "function identity(value:any):any {return value;} export function check(value:number):number {return identity(value);}",
        "type Value=any; function identity(value:Value):Value {return value;} export function check(value:number):number {return identity(value);}",
        "export function check(value:number):number {return value as any;}",
        "export function check(value:number):number {let local:any=value;return local;}",
        "function unused(value){return value;} export function check(value:number):number {return value;}",
    ] {
        assert!(compile_world(source, wit).is_err(), "{source}");
    }
}

#[test]
fn nullable_and_variant_values_preserve_payloads_and_flat_join_bits() -> Result<()> {
    let source = r#"
        type Choice={tag:'number';val:number}|{tag:'text';val:string}|{tag:'absent'};
        interface Packet { label:string|null|undefined; enabled:boolean|null|undefined; choice:Choice; }
        export function echo(value:Packet):Packet {return value;}
        export function choose(value:Choice):Choice {return value;}
        export function fallback(value:string|null|undefined):string {
            if(value===null || value===undefined) {return 'visual';}
            return value;
        }
    "#;
    let compiled = compile_world(
        source,
        "package test:boundary; world boundary {
        variant choice { number(f64), text(string), absent }
        record packet { label:option<string>, enabled:option<bool>, choice:choice }
        export echo:func(value:packet)->packet;
        export choose:func(value:choice)->choice;
        export fallback:func(value:option<string>)->string;
    }",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let echo = instance.get_func(&mut store, "echo").unwrap();
    let choose = instance.get_func(&mut store, "choose").unwrap();
    let fallback =
        instance.get_typed_func::<(Option<String>,), (String,)>(&mut store, "fallback")?;
    use wasmtime::component::Val;
    for _ in 0..100 {
        for choice in [
            Val::Variant("number".into(), Some(Box::new(Val::Float64(-123.25)))),
            Val::Variant("text".into(), Some(Box::new(Val::String("漢字🙂".into())))),
            Val::Variant("absent".into(), None),
        ] {
            let mut results = [Val::Bool(false)];
            choose.call(&mut store, std::slice::from_ref(&choice), &mut results)?;
            assert_eq!(results[0], choice);
            for (label, enabled) in [
                (None, None),
                (
                    Some(Box::new(Val::String(String::new()))),
                    Some(Box::new(Val::Bool(false))),
                ),
                (
                    Some(Box::new(Val::String("location".into()))),
                    Some(Box::new(Val::Bool(true))),
                ),
            ] {
                let input = Val::Record(vec![
                    ("label".into(), Val::Option(label)),
                    ("enabled".into(), Val::Option(enabled)),
                    ("choice".into(), choice.clone()),
                ]);
                echo.call(&mut store, std::slice::from_ref(&input), &mut results)?;
                assert_eq!(results[0], input);
            }
        }
        assert_eq!(fallback.call(&mut store, (None,))?.0, "visual");
        assert_eq!(fallback.call(&mut store, (Some(String::new()),))?.0, "");
        assert_eq!(
            fallback.call(&mut store, (Some("漢字🙂".into()),))?.0,
            "漢字🙂"
        );
    }
    Ok(())
}

#[test]
fn imports_preserve_missing_empty_and_variant_error_values() -> Result<()> {
    use wasmtime::component::Val;
    let wit_dir = std::path::Path::new("tests/fixtures/optional-import");
    let (resolve, package) = perry_wit::component::wit::resolve_wit(wit_dir)?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    let compiled = compile_typescript_for_world(
        include_str!("fixtures/optional_import.ts"),
        "optional_import.ts",
        &WaffleCompileOptions::default(),
        resolve,
        world,
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    linker.instance("test:lookup/service")?.func_new(
        "lookup",
        |_store, _ty, params, results| {
            let Val::String(key) = &params[0] else {
                unreachable!()
            };
            if key == "trap" {
                return Err(wasmtime::Error::msg("injected configuration host failure"));
            }
            results[0] = match key.as_str() {
                "denied" => Val::Result(Err(Some(Box::new(Val::Variant("denied".into(), None))))),
                "missing" => Val::Result(Ok(Some(Box::new(Val::Option(None))))),
                "empty" => Val::Result(Ok(Some(Box::new(Val::Option(Some(Box::new(
                    Val::String(String::new()),
                ))))))),
                "offline" => Val::Result(Err(Some(Box::new(Val::Variant(
                    key.clone(),
                    Some(Box::new(Val::String("漢字🙂".into()))),
                ))))),
                _ => Val::Result(Ok(Some(Box::new(Val::Option(Some(Box::new(
                    Val::String("checkout".into()),
                ))))))),
            };
            Ok(())
        },
    )?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = linker.instantiate(&mut store, &component)?;
    let read = instance.get_typed_func::<(String,), (String,)>(&mut store, "read")?;
    for _ in 0..100 {
        for (key, expected) in [
            ("missing", "fallback"),
            ("empty", ""),
            ("present", "checkout"),
            ("denied", "denied"),
            ("offline", "offline: 漢字🙂"),
        ] {
            assert_eq!(read.call(&mut store, (key.into(),))?.0, expected);
        }
    }
    let error = read.call(&mut store, ("trap".into(),)).unwrap_err();
    assert!(format!("{error:#}").contains("injected configuration host failure"));
    Ok(())
}

#[test]
fn configuration_source_matches_the_generated_import_sdk() -> Result<()> {
    check_sdk_source(
        include_str!("fixtures/optional-import/world.wit"),
        include_str!("fixtures/optional_import.ts"),
    )
}

#[test]
fn wit_imports_preserve_indirect_values_flat_joins_unit_results_and_live_owners() -> Result<()> {
    const WIT: &str = "package test:imports;
        interface service {
            variant choice { number(f64), text(string), absent }
            record packet { label:string, enabled:option<bool>, choice:choice, values:tuple<u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32> }
            exchange:func(value:packet)->packet;
            choose:func(value:choice)->choice;
            apply:func(value:bool)->result<_,string>;
            double:func(value:f64)->f64;
            notify:func();
        }
        interface checkpoint {}
        world boundary {
            import service;
            export checkpoint;
            use service.{packet,choice};
            export relay:func(value:packet)->packet;
            export select:func(value:choice)->choice;
            export checked:func(value:bool)->string;
            export twice:func(value:f64)->f64;
        }";
    let source = r#"
        import * as host from "test:imports/service";
        import type { Packet, Choice } from "test:imports/service";
        export function relay(value:Packet):Packet {
            const retained=host.exchange(value);
            let index=0;
            while(index<300) {const discarded=host.exchange(value); index=index+1;}
            return retained;
        }
        export function select(value:Choice):Choice {return host['choose'](value);}
        export function checked(value:boolean):string {
            const result=host.apply(value);
            if(!result.ok) {return result.error;}
            host.notify();
            return "done";
        }
        export function twice(value:number):number {return host.double(value);}
    "#;
    let compiled = compile_world(source, WIT)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    let mut service = linker.instance("test:imports/service")?;
    for function in ["exchange", "choose"] {
        service.func_new(function, |_store, _ty, params, results| {
            results[0] = params[0].clone();
            Ok(())
        })?;
    }
    use wasmtime::component::Val;
    service.func_new("apply", |_store, _ty, params, results| {
        results[0] = if params[0] == Val::Bool(true) {
            Val::Result(Ok(None))
        } else {
            Val::Result(Err(Some(Box::new(Val::String("拒否🙂".into())))))
        };
        Ok(())
    })?;
    service.func_wrap(
        "double",
        |_store: wasmtime::StoreContextMut<'_, StoreLimits>, (value,): (f64,)| Ok((value * 2.,)),
    )?;
    service.func_wrap(
        "notify",
        |_store: wasmtime::StoreContextMut<'_, StoreLimits>, (): ()| Ok(()),
    )?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = linker.instantiate(&mut store, &component)?;
    let relay = instance.get_func(&mut store, "relay").unwrap();
    let select = instance.get_func(&mut store, "select").unwrap();
    let checked = instance.get_typed_func::<(bool,), (String,)>(&mut store, "checked")?;
    let twice = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "twice")?;
    for choice in [
        Val::Variant("number".into(), Some(Box::new(Val::Float64(-123.25)))),
        Val::Variant("text".into(), Some(Box::new(Val::String("漢字🙂".into())))),
        Val::Variant("absent".into(), None),
    ] {
        let packet = Val::Record(vec![
            ("label".into(), Val::String("owner漢字🙂".repeat(20))),
            (
                "enabled".into(),
                Val::Option(Some(Box::new(Val::Bool(false)))),
            ),
            ("choice".into(), choice.clone()),
            (
                "values".into(),
                Val::Tuple((0..18).map(|i| Val::U32(u32::MAX - i)).collect()),
            ),
        ]);
        let mut results = [Val::Bool(false)];
        for _ in 0..10 {
            relay.call(&mut store, std::slice::from_ref(&packet), &mut results)?;
            assert_eq!(results[0], packet);
            select.call(&mut store, std::slice::from_ref(&choice), &mut results)?;
            assert_eq!(results[0], choice);
            assert_eq!(checked.call(&mut store, (true,))?.0, "done");
            assert_eq!(checked.call(&mut store, (false,))?.0, "拒否🙂");
            assert_eq!(twice.call(&mut store, (1.25,))?.0, 2.5);
        }
    }
    Ok(())
}

#[test]
fn wit_import_binding_identity_and_static_call_diagnostics() -> Result<()> {
    const WIT: &str = "package test:bindings; interface service { echo:func(value:string)->string; } world boundary {import service; export run:func(value:string)->string;}";
    let compiled = compile_world(
        r#"
        import {echo as send} from "test:bindings/service";
        function shadow(send:string):string {return send;}
        export function run(value:string):string {return send(shadow(value));}
    "#,
        WIT,
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    linker.instance("test:bindings/service")?.func_wrap(
        "echo",
        |_store: wasmtime::StoreContextMut<'_, ()>, (value,): (String,)| Ok((value + "!",)),
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    assert_eq!(
        instance
            .get_typed_func::<(String,), (String,)>(&mut store, "run")?
            .call(&mut store, ("scope".into(),))?
            .0,
        "scope!"
    );
    for (source, diagnostic) in [
        (
            r#"import {echo} from "test:bindings/service"; export function run(value:string):string {const escaped=echo;return escaped(value);}"#,
            "direct calls",
        ),
        (
            r#"import * as host from "test:bindings/service"; export function run(value:string):string {const escaped=host;return escaped.echo(value);}"#,
            "direct member calls",
        ),
        (
            r#"import * as host from "test:bindings/service"; export function run(value:string):string {return host[value](value);}"#,
            "static member name",
        ),
        (
            r#"import {echo} from "test:bindings/service"; export function run(value:string):string {return echo(...[value]);}"#,
            "Spread WIT arguments",
        ),
        (
            r#"import host from "test:bindings/service"; export function run(value:string):string {return value;}"#,
            "no default export",
        ),
        (
            r#"import "test:bindings/service"; export function run(value:string):string {return value;}"#,
            "module initialization",
        ),
        (
            r#"import {echo} from "test:bindings/service"; export function run(value:string):string {return echo(42);}"#,
            "static type",
        ),
    ] {
        let Err(error) = compile_world(source, WIT) else {
            panic!("{source}");
        };
        let error = format!("{error:#}");
        assert!(error.contains(diagnostic), "{error}");
    }
    Ok(())
}

#[test]
fn typed_lists_preserve_nested_values_and_dense_mutation_under_collection() -> Result<()> {
    const WIT: &str = "package test:lists; interface host {
        record row {name:string,enabled:option<bool>,labels:list<string>,values:list<f64>}
        echo:func(rows:list<row>)->list<row>;
    } world boundary {import host; use host.{row};
        export transform:func(rows:list<row>)->list<row>;
        export bytes:func(value:list<u8>)->list<u8>;
    }";
    let source = r#"
        import {echo} from "test:lists/host";
        import type {Row} from "test:lists/host";
        function label(value:string):string {
            let index=0;
            while(index<100) {const discarded=value+'x';index=index+1;}
            return '追加🙂';
        }
        export function transform(rows:Row[]):Row[] {
            const output:Row[]=[];
            let index=0;
            while(index<rows.length) {
                const row=rows[index];
                const values:number[]=[1,2];
                values.push(index);
                values[0]=row.values[0];
                output.push({name:row.name,enabled:row.enabled,labels:[row.name+'!',label(row.name)],values:values});
                index=index+1;
            }
            const retained=echo(output);
            index=0;
            while(index<500) {const discarded=echo(output);index=index+1;}
            return retained;
        }
        export function bytes(value:Uint8Array):Uint8Array {return value;}
    "#;
    check_sdk_source(WIT, source)?;
    let compiled = compile_world(source, WIT)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    linker
        .instance("test:lists/host")?
        .func_new("echo", |_store, _ty, params, results| {
            results[0] = params[0].clone();
            Ok(())
        })?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(524288).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = linker.instantiate(&mut store, &component)?;
    let transform = instance.get_func(&mut store, "transform").unwrap();
    use wasmtime::component::Val;
    let row = |index: u32, transformed: bool| {
        Val::Record(vec![
            ("name".into(), Val::String(format!("品🙂{index}"))),
            (
                "enabled".into(),
                Val::Option(Some(Box::new(Val::Bool(false)))),
            ),
            (
                "labels".into(),
                Val::List(if transformed {
                    vec![
                        Val::String(format!("品🙂{index}!")),
                        Val::String("追加🙂".into()),
                    ]
                } else {
                    vec![]
                }),
            ),
            (
                "values".into(),
                Val::List(if transformed {
                    vec![
                        Val::Float64(-1.25),
                        Val::Float64(2.),
                        Val::Float64(index as f64),
                    ]
                } else {
                    vec![Val::Float64(-1.25)]
                }),
            ),
        ])
    };
    for _ in 0..8 {
        for count in [0, 1, 12] {
            let input = Val::List((0..count).map(|index| row(index, false)).collect());
            let mut output = [Val::Bool(false)];
            transform.call(&mut store, &[input], &mut output)?;
            assert_eq!(
                output[0],
                Val::List((0..count).map(|index| row(index, true)).collect())
            );
        }
    }
    let bytes = instance.get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "bytes")?;
    assert_eq!(
        bytes.call(&mut store, ((0..=255).collect(),))?.0,
        (0..=255).collect::<Vec<_>>()
    );
    Ok(())
}

#[test]
fn typed_list_bounds_and_invalid_mutations_do_not_create_holes() -> Result<()> {
    const WIT: &str = "package test:bounds; world boundary {export run:func(index:f64)->result<list<f64>,list<f64>>;}";
    let source = r#"
        type Outcome={ok:true;value:number[]}|{ok:false;error:number[]};
        export function run(index:number):Outcome {
            const values:number[]=[10,20];
            const alias=values;
            try {alias[index]=30;const value=values[index];return {ok:true,value:values};}
            catch(error) {return {ok:false,error:values};}
        }
    "#;
    let compiled = compile_world(source, WIT)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run =
        instance.get_typed_func::<(f64,), (Result<Vec<f64>, Vec<f64>>,)>(&mut store, "run")?;
    for (index, expected) in [
        (0., Ok(vec![30., 20.])),
        (1., Ok(vec![10., 30.])),
        (2., Err(vec![10., 20.])),
        (-1., Err(vec![10., 20.])),
        (0.5, Err(vec![10., 20.])),
        (f64::NAN, Err(vec![10., 20.])),
        (f64::INFINITY, Err(vec![10., 20.])),
    ] {
        assert_eq!(run.call(&mut store, (index,))?.0, expected);
    }
    for mutation in [
        "alias.push('wrong')",
        "alias[index]='wrong'",
        "alias.length=10",
        "alias[-1]='wrong'",
        "delete alias[0]",
        "(alias as string[]).push('wrong')",
    ] {
        assert!(
            compile_world(&source.replace("alias[index]=30", mutation), WIT).is_err(),
            "{mutation}"
        );
    }
    assert!(compile_world(&source.replace("[10,20]", "[10,,20]"), WIT).is_err());
    assert!(compile_world(&source.replace("[10,20]", "[10,'wrong']"), WIT).is_err());
    Ok(())
}

#[test]
fn telemetry_scalar_transport_preserves_u64_precision_and_optional_flags() -> Result<()> {
    const WIT: &str = "package test:telemetry; interface host {
        flags trace-flags {sampled}
        record datetime {seconds:u64,nanoseconds:u32}
        record packet {timestamp:option<datetime>,trace-flags:option<trace-flags>}
        echo:func(packet:packet)->packet;
    } world boundary {import host; use host.{packet,trace-flags};
        export echo:func(packet:packet)->packet;
        export select:func(enabled:option<bool>)->trace-flags;
    }";
    let source = r#"import {echo as hostEcho} from "test:telemetry/host"; import type {Packet,TraceFlags} from "test:telemetry/host";
        export function echo(packet:Packet):Packet {const retained=hostEcho(packet);let index=0;while(index<500){const discarded=hostEcho(packet);index=index+1;}return retained;}
        export function select(enabled:boolean|null|undefined):TraceFlags {
            if(enabled===null||enabled===undefined) {return {};}
            return {sampled:enabled};
        }"#;
    check_sdk_source(WIT, source)?;
    let compiled = compile_world(source, WIT)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    linker
        .instance("test:telemetry/host")?
        .func_new("echo", |_store, _ty, params, results| {
            results[0] = params[0].clone();
            Ok(())
        })?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = linker.instantiate(&mut store, &component)?;
    let echo = instance.get_func(&mut store, "echo").unwrap();
    use wasmtime::component::Val;
    let select = instance.get_func(&mut store, "select").unwrap();
    for enabled in [None, Some(false), Some(true)] {
        let mut result = [Val::Bool(false)];
        select.call(
            &mut store,
            &[Val::Option(enabled.map(|value| Box::new(Val::Bool(value))))],
            &mut result,
        )?;
        assert_eq!(
            result[0],
            Val::Flags(if enabled == Some(true) {
                vec!["sampled".into()]
            } else {
                vec![]
            })
        );
    }
    for seconds in [0, 1, 9007199254740993, u64::MAX] {
        for flags in [
            None,
            Some(Box::new(Val::Flags(vec![]))),
            Some(Box::new(Val::Flags(vec!["sampled".into()]))),
        ] {
            let packet = Val::Record(vec![
                (
                    "timestamp".into(),
                    Val::Option(Some(Box::new(Val::Record(vec![
                        ("seconds".into(), Val::U64(seconds)),
                        ("nanoseconds".into(), Val::U32(999999999)),
                    ])))),
                ),
                ("trace-flags".into(), Val::Option(flags)),
            ]);
            let mut result = [Val::Bool(false)];
            echo.call(&mut store, std::slice::from_ref(&packet), &mut result)?;
            assert_eq!(result[0], packet);
        }
    }
    for operation in [
        "return value+value;",
        "value++;return value;",
        "return -value;",
        "return +value;",
        "if(value){return value;}return value;",
        "if(value===value){return value;}return value;",
        "return 1n;",
    ] {
        let source = format!("export function run(value:bigint):bigint {{{operation}}}");
        assert!(
            compile_world(
                &source,
                "package test:bigint; world boundary {export run:func(value:u64)->u64;}"
            )
            .is_err(),
            "{operation}"
        );
    }
    Ok(())
}

#[test]
fn module_bindings_initialize_once_and_survive_collection_across_exports() -> Result<()> {
    let compiled = compile_world(
        r#"
        let count:number=10;
        let label:string='retained 漢🙂';
        function setup():number {let i=0;while(i<600){const unused=label+' temporary';i++;} count=count+1;return count;}
        const initial:number=setup();
        export function next(value:string):string {
            let i=0;
            while(i<600){const temporary=value+' discarded'; i=i+1;}
            count=count+1;
            label=label+'!';
            return value+label;
        }
        export function total():number {return count+initial;}
        "#,
        "package test:boundary; world boundary {export next:func(value:string)->string; export total:func()->f64;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    for _ in 0..2 {
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(262144).build(),
        );
        store.limiter(|limits: &mut StoreLimits| limits);
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let next = instance.get_typed_func::<(String,), (String,)>(&mut store, "next")?;
        let total = instance.get_typed_func::<(), (f64,)>(&mut store, "total")?;
        let mut label = String::from("retained 漢🙂");
        for index in 0..50 {
            label.push('!');
            assert_eq!(
                next.call(&mut store, ("input ".repeat(100),))?.0,
                "input ".repeat(100) + label.as_str()
            );
            assert_eq!(total.call(&mut store, ())?.0, 23.0 + index as f64);
        }
    }
    Ok(())
}

#[test]
fn initialization_reads_observe_the_temporal_dead_zone() -> Result<()> {
    let compiled = compile_world(
        "const first:number=read(); let later:number=4; function read():number{return later;} export function value():number{return first;}",
        "package test:boundary; world boundary {export value:func()->f64;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let value = instance.get_typed_func::<(), (f64,)>(&mut store, "value")?;
    assert!(value.call(&mut store, ()).is_err());
    Ok(())
}

#[test]
fn top_level_await_requires_asynchronous_wit_exports() {
    let error = compile_world(
        "await 1; export function value():number{return 2;}",
        "package test:boundary; world boundary {export value:func()->f64;}",
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("async func"), "{error:#}");
}

#[test]
fn component_compilation_requires_an_explicit_world() -> Result<()> {
    let source = "export function run():number {return 42;}";
    let result =
        perry_wit::compile_typescript_waffle(source, "entry.ts", &WaffleCompileOptions::default());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("explicitly resolved WIT world")
    );
    let core = perry_wit::compile_typescript_waffle(
        source,
        "entry.ts",
        &WaffleCompileOptions {
            componentize: false,
            ..Default::default()
        },
    )?;
    assert!(core.component.is_none());
    assert!(core.component_wat.is_none());
    wasmparser::Validator::new().validate_all(&core.core)?;
    Ok(())
}

/// Signed clock instants and unsigned identifiers must retain every bit across both ABI directions.
#[test]
fn signed_bigint_transport_preserves_scalar_and_nested_instants() -> Result<()> {
    const WIT: &str = "package test:signed-clock;
      interface host {
        record instant { seconds:s64, nanoseconds:u32 }
        record packet { timestamp:option<instant>, samples:list<s64>, mixed:tuple<s64,u64> }
        scalar:func(value:s64)->s64;
        nested:func(value:packet)->packet;
      }
      world boundary { import host; use host.{packet};
        export scalar:func(value:s64)->s64;
        export nested:func(value:packet)->packet;
      }";
    let source = r#"import {scalar as scalarHost,nested as nestedHost} from 'test:signed-clock/host';
      import type {Packet} from 'test:signed-clock/host';
      export function scalar(value:bigint):bigint {return scalarHost(value);}
      export function nested(value:Packet):Packet {return nestedHost(value);}"#;
    check_sdk_source(WIT, source)?;
    let compiled = compile_world(source, WIT)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    for function in ["scalar", "nested"] {
        linker.instance("test:signed-clock/host")?.func_new(
            function,
            |_, _, params, results| {
                results[0] = params[0].clone();
                Ok(())
            },
        )?;
    }
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let scalar = instance.get_typed_func::<(i64,), (i64,)>(&mut store, "scalar")?;
    let nested = instance.get_func(&mut store, "nested").unwrap();
    for seconds in [
        i64::MIN,
        -9_007_199_254_740_993,
        -1,
        0,
        1,
        9_007_199_254_740_993,
        i64::MAX,
    ] {
        assert_eq!(scalar.call(&mut store, (seconds,))?, (seconds,));
        for timestamp in [
            None,
            Some(Box::new(Val::Record(vec![
                ("seconds".into(), Val::S64(seconds)),
                ("nanoseconds".into(), Val::U32(999_999_999)),
            ]))),
        ] {
            let packet = Val::Record(vec![
                ("timestamp".into(), Val::Option(timestamp)),
                (
                    "samples".into(),
                    Val::List(vec![Val::S64(seconds), Val::S64(-1)]),
                ),
                (
                    "mixed".into(),
                    Val::Tuple(vec![Val::S64(seconds), Val::U64(u64::MAX)]),
                ),
            ]);
            let mut results = [Val::Bool(false)];
            nested.call(&mut store, std::slice::from_ref(&packet), &mut results)?;
            assert_eq!(results[0], packet);
        }
    }
    Ok(())
}

/// Builtin shadow renaming changes bindings while retaining literal and declared record keys.
#[test]
fn builtin_names_remain_literal_record_fields() -> Result<()> {
    const WIT: &str = "package test:property-names;
      interface types { record metadata {fetch:string,process:string,console:string} }
      world boundary {use types.{metadata}; export run:func()->list<metadata>;}";
    let source = r#"interface Metadata {fetch:string;process:string;console:string}
      export function run():Metadata[] {
        const fetch:string='get'; const process:string='local'; const console:string='literal';
        return [{fetch:'explicit',process:'named',console:'fields'},{fetch,process,console}];
      }"#;
    check_sdk_source(WIT, source)?;
    let compiled = compile_world(source, WIT)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_func(&mut store, "run").unwrap();
    let mut results = [Val::Bool(false)];
    run.call(&mut store, &[], &mut results)?;
    let record = |fetch: &str, process: &str, console: &str| {
        Val::Record(vec![
            ("fetch".into(), Val::String(fetch.into())),
            ("process".into(), Val::String(process.into())),
            ("console".into(), Val::String(console.into())),
        ])
    };
    assert_eq!(
        results[0],
        Val::List(vec![
            record("explicit", "named", "fields"),
            record("get", "local", "literal")
        ])
    );
    Ok(())
}

#[test]
fn text_encoder_returns_independent_utf8_bytes() -> Result<()> {
    let compiled = compile_world(
        "export function run(value:string):Uint8Array { const bytes=new TextEncoder().encode(value); const other=new TextEncoder().encode(value); if(other.length>0) other[0]=0; return bytes; }",
        "package test:boundary; world boundary {export run:func(value:string)->list<u8>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&str,), (Vec<u8>,)>(&mut store, "run")?;
    for value in ["", "ASCII", "Grüße 漢字🙂"] {
        assert_eq!(run.call(&mut store, (value,))?.0, value.as_bytes());
    }
    Ok(())
}

const RESOURCE_WIT: &str = "package test:boundary;
interface secrets {
    resource secret;
    get: func(value:u32)->secret;
    reveal: func(value:borrow<secret>)->u32;
    take: func(value:secret)->u32;
}
world boundary { import secrets; export run:func(value:u32)->u32; }";

#[test]
fn imported_resources_borrow_transfer_and_dispose() -> Result<()> {
    use wasmtime::component::{Resource, ResourceType};
    struct Secret;
    for (body, expected, drops) in [
        (
            "const a=get(value); const b=reveal(a); dropSecret(a); return b;",
            42,
            1,
        ),
        ("const a=get(value); return take(a);", 42, 0),
    ] {
        let source = format!(
            "import {{get,reveal,take,dropSecret}} from 'test:boundary/secrets'; export function run(value:number):number {{{body}}}"
        );
        check_sdk_source(RESOURCE_WIT, &source)?;
        let compiled = compile_world(&source, RESOURCE_WIT)?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, 0u32);
        let mut linker = Linker::new(&engine);
        let mut secrets = linker.instance("test:boundary/secrets")?;
        secrets.resource("secret", ResourceType::host::<Secret>(), |mut store, _| {
            *store.data_mut() += 1;
            Ok(())
        })?;
        secrets.func_wrap("get", |_, (value,): (u32,)| {
            Ok((Resource::<Secret>::new_own(value),))
        })?;
        secrets.func_wrap("reveal", |_, (value,): (Resource<Secret>,)| {
            assert!(!value.owned());
            Ok((value.rep(),))
        })?;
        secrets.func_wrap("take", |_, (value,): (Resource<Secret>,)| {
            assert!(value.owned());
            Ok((value.rep(),))
        })?;
        let instance = linker.instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (42,))?.0, expected);
        assert_eq!(*store.data(), drops);
    }
    Ok(())
}

#[test]
fn consumed_resource_aliases_trap_before_reaching_host() -> Result<()> {
    use wasmtime::component::{Resource, ResourceType};
    struct Secret;
    for action in ["dropSecret(a)", "take(a)"] {
        let source = format!(
            "import {{get,reveal,take,dropSecret}} from 'test:boundary/secrets'; export function run(value:number):number {{const a=get(value); const alias=a; {action}; return reveal(alias);}}"
        );
        let compiled = compile_world(&source, RESOURCE_WIT)?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        let mut secrets = linker.instance("test:boundary/secrets")?;
        secrets.resource("secret", ResourceType::host::<Secret>(), |_, _| Ok(()))?;
        secrets.func_wrap("get", |_, (value,): (u32,)| {
            Ok((Resource::<Secret>::new_own(value),))
        })?;
        secrets.func_wrap(
            "reveal",
            |_, (_value,): (Resource<Secret>,)| -> wasmtime::Result<(u32,)> {
                panic!("use-after-consume reached host")
            },
        )?;
        secrets.func_wrap("take", |_, (value,): (Resource<Secret>,)| {
            Ok((value.rep(),))
        })?;
        let instance = linker.instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
        assert!(run.call(&mut store, (42,)).is_err());
    }
    Ok(())
}

#[test]
fn exported_resources_preserve_representations_and_expire_borrows() -> Result<()> {
    use wasmtime::component::ResourceAny;
    let wit = "package test:boundary; interface secrets {resource secret; get:func(value:u32)->secret; reveal:func(value:borrow<secret>)->u32;} world boundary {export secrets;}";
    let source = "import type {Secret} from 'test:boundary/secrets'; import {newSecret,secretRep} from 'test:boundary/secrets'; export function secretsGet(value:number):Secret {return newSecret(value);} export function secretsReveal(value:Secret):number {return secretRep(value);}";
    check_sdk_source(wit, source)?;
    let compiled = compile_world(source, wit)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let (_, interface) = instance
        .get_export(&mut store, None, "test:boundary/secrets")
        .unwrap();
    let (_, get) = instance
        .get_export(&mut store, Some(&interface), "get")
        .unwrap();
    let (_, reveal) = instance
        .get_export(&mut store, Some(&interface), "reveal")
        .unwrap();
    let get = instance.get_typed_func::<(u32,), (ResourceAny,)>(&mut store, get)?;
    let reveal = instance.get_typed_func::<(ResourceAny,), (u32,)>(&mut store, reveal)?;
    for value in [0, 42, u32::MAX] {
        let secret = get.call(&mut store, (value,))?.0;
        for _ in 0..3 {
            assert_eq!(reveal.call(&mut store, (secret,))?.0, value);
        }
        secret.resource_drop(&mut store)?;
    }
    Ok(())
}

#[test]
fn outbound_bytes_accept_utf8_text_and_keep_inbound_bytes_mutable() -> Result<()> {
    let wit = "package test:boundary; interface sink { record document {bytes:list<u8>} write:func(value:document)->list<u8>; } world boundary {import sink; export run:func(input:list<u8>)->list<u8>;}";
    for source in [
        "import {write} from 'test:boundary/sink'; export function run(input:Uint8Array):string { input[0]=65; const copy=write({bytes:'Grüße 🙂'}); copy[0]=66; return 'Grüße 🙂';}",
        "import {write} from 'test:boundary/sink'; import type {DocumentInput} from 'test:boundary/sink'; export function run(input:Uint8Array):Uint8Array {const document:DocumentInput={bytes:'Grüße 🙂'}; return write(document);}",
    ] {
        check_sdk_source(wit, source)?;
        let compiled = compile_world(source, wit)?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        linker
            .instance("test:boundary/sink")?
            .func_new("write", |_, _, args, results| {
                let Val::Record(fields) = &args[0] else {
                    panic!()
                };
                let Val::List(bytes) = &fields[0].1 else {
                    panic!()
                };
                let bytes: Vec<u8> = bytes
                    .iter()
                    .map(|b| match b {
                        Val::U8(b) => *b,
                        _ => panic!(),
                    })
                    .collect();
                assert_eq!(bytes, "Grüße 🙂".as_bytes());
                results[0] = fields[0].1.clone();
                Ok(())
            })?;
        let instance = linker.instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "run")?;
        for _ in 0..200 {
            assert_eq!(run.call(&mut store, (vec![0],))?.0, "Grüße 🙂".as_bytes());
        }
    }
    Ok(())
}

#[test]
fn numeric_bigint_construction_preserves_safe_integers_and_rejects_invalid_values() -> Result<()> {
    let compiled = compile_world(
        "export function run(value:number):bigint|null|undefined {try {return BigInt(value);} catch {return null;}}",
        "package test:boundary; world boundary {export run:func(value:f64)->option<s64>;}",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(f64,), (Option<i64>,)>(&mut store, "run")?;
    for value in [
        -9007199254740991.0,
        -42.0,
        0.0,
        1791230000.0,
        9007199254740991.0,
    ] {
        assert_eq!(run.call(&mut store, (value,))?.0, Some(value as i64));
    }
    for value in [f64::NAN, f64::INFINITY, 1.5, 9007199254740992.0] {
        assert_eq!(run.call(&mut store, (value,))?.0, None);
    }
    Ok(())
}

#[test]
fn nested_outbound_byte_lists_support_text_arrays_and_optional_payloads() -> Result<()> {
    let wit = "package test:boundary; interface documents {record packet {body:list<u8>, chunks:list<list<u8>>, optional:option<list<u8>>} } world boundary {use documents.{packet}; export run:func()->packet;}";
    for source in [
        "export function run():{body:string,chunks:string[],optional:string|null|undefined} {return {body:'ä🙂',chunks:['one','漢字'],optional:'yes'};}",
        "import type {PacketInput} from 'test:boundary/documents'; export function run():PacketInput {return {body:'ä🙂',chunks:['one','漢字'],optional:'yes'};}",
    ] {
        check_sdk_source(wit, source)?;
        let compiled = compile_world(source, wit)?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let run = instance.get_func(&mut store, "run").unwrap();
        let mut result = [Val::Bool(false)];
        run.call(&mut store, &[], &mut result)?;
        let bytes = |s: &str| Val::List(s.as_bytes().iter().map(|b| Val::U8(*b)).collect());
        assert_eq!(
            result[0],
            Val::Record(vec![
                ("body".into(), bytes("ä🙂")),
                (
                    "chunks".into(),
                    Val::List(vec![bytes("one"), bytes("漢字")])
                ),
                ("optional".into(), Val::Option(Some(Box::new(bytes("yes")))))
            ])
        );
    }
    Ok(())
}

#[test]
fn borrowed_imported_resources_expire_when_the_export_returns() -> Result<()> {
    use wasmtime::component::{Resource, ResourceType};
    struct Secret;
    let wit = "package test:boundary; interface secrets {resource secret; reveal:func(value:borrow<secret>)->u32;} world boundary {import secrets;use secrets.{secret};export remember:func(value:borrow<secret>);export stale:func()->u32;}";
    let source = "import type {Secret} from 'test:boundary/secrets';import {reveal} from 'test:boundary/secrets';let saved:Secret|null=null;export function remember(value:Secret):void {saved=value;}export function stale():number {const value=saved;if(value===null)return 0;return reveal(value);}";
    let compiled = compile_world(source, wit)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    let mut secrets = linker.instance("test:boundary/secrets")?;
    secrets.resource("secret", ResourceType::host::<Secret>(), |_, _| Ok(()))?;
    secrets.func_wrap(
        "reveal",
        |_, (_value,): (Resource<Secret>,)| -> wasmtime::Result<(u32,)> {
            panic!("expired borrow reached host")
        },
    )?;
    let instance = linker.instantiate(&mut store, &component)?;
    let remember = instance.get_typed_func::<(Resource<Secret>,), ()>(&mut store, "remember")?;
    remember.call(&mut store, (Resource::new_borrow(42),))?;
    let stale = instance.get_typed_func::<(), (u32,)>(&mut store, "stale")?;
    assert!(stale.call(&mut store, ()).is_err());
    Ok(())
}
