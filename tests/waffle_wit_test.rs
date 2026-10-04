use anyhow::Result;
use perry_wit::waffle_backend::{
    WaffleCompileOptions, WaffleCompiled, compile_typescript_for_world,
};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};

mod records {
    wasmtime::component::bindgen!({path: "tests/fixtures/record-boundary", world: "boundary"});
}

fn compile_records(source: &str) -> Result<WaffleCompiled> {
    compile_world(source, include_str!("fixtures/record-boundary/world.wit"))
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
