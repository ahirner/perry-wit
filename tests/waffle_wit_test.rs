use anyhow::Result;
use perry_wit::waffle_backend::{
    WaffleCompileOptions, WaffleCompiled, compile_typescript_for_world,
};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};

mod calendar {
    wasmtime::component::bindgen!({path: "tests/fixtures/calendar-wit", world: "component"});
}

fn compile(source: &str) -> Result<WaffleCompiled> {
    let (resolve, package) = perry_wit::component::wit::resolve_wit(std::path::Path::new(
        "tests/fixtures/calendar-wit",
    ))?;
    let world = resolve.select_world(&[package], Some("component"))?;
    compile_typescript_for_world(
        source,
        "calendar.ts",
        &WaffleCompileOptions::default(),
        resolve,
        world,
    )
}

fn iso(text: &str) -> calendar::exports::workflow::calendar::dates::IsoDate {
    let b = text.as_bytes();
    (b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9])
}

#[test]
fn calendar_implements_the_production_wit_world_under_bounded_memory() -> Result<()> {
    use calendar::exports::workflow::calendar::dates::{Error, Shift};
    let compiled = compile(include_str!("fixtures/calendar_component.ts"))?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(262144).build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    let instance = calendar::Component::instantiate(&mut store, &component, &Linker::new(&engine))?;
    let dates = instance.workflow_calendar_dates();
    for _ in 0..100 {
        for (date, days, expected) in [
            ("2024-02-28", 1, Ok(iso("2024-02-29"))),
            ("2024-02-29", 1, Ok(iso("2024-03-01"))),
            ("2000-03-01", -1, Ok(iso("2000-02-29"))),
            ("1900-03-01", -1, Ok(iso("1900-02-28"))),
            ("1970-01-01", -1, Ok(iso("1969-12-31"))),
            ("0001-01-01", 0, Ok(iso("0001-01-01"))),
            ("9999-12-31", 0, Ok(iso("9999-12-31"))),
            ("0000-01-01", 0, Err(Error::InvalidDate)),
            ("2023-02-29", 0, Err(Error::InvalidDate)),
            ("2024/01/01", 0, Err(Error::InvalidDate)),
            ("9999-12-31", 1, Err(Error::OutOfRange)),
            ("0001-01-01", -1, Err(Error::OutOfRange)),
            ("2000-01-01", i32::MAX, Err(Error::OutOfRange)),
            ("2000-01-01", i32::MIN, Err(Error::OutOfRange)),
        ] {
            assert_eq!(
                dates.call_offset(
                    &mut store,
                    Shift {
                        date: iso(date),
                        days
                    }
                )?,
                expected
            );
        }
    }
    Ok(())
}

#[test]
fn calendar_source_matches_the_generated_production_sdk() -> Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::write(
        project.path().join("calendar.ts"),
        include_str!("fixtures/calendar_component.ts"),
    )?;
    let sdk = perry_wit::generate_sdk_files(&perry_wit::SdkOptions {
        wit_dir: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/calendar-wit"),
        world: Some("component".into()),
        out_dir: project.path().join("types"),
        project_root: Some(project.path().into()),
        entry: "calendar.ts".into(),
    })?;
    let output = std::process::Command::new("tsc")
        .args([
            "--ignoreConfig",
            "--noEmit",
            "--strict",
            "--target",
            "ES2022",
            "--module",
            "esnext",
        ])
        .arg(sdk.check_path)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/types/p3.d.ts"))
        .output()?;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

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
    let source = include_str!("fixtures/calendar_component.ts");
    for (from, to) in [
        ("datesOffset", "wrongName"),
        ("days: number", "days: string"),
        ("date: IsoDate", "wrong: IsoDate"),
        (
            "number, number, number, number, number, number, number, number, number, number",
            "number, number",
        ),
        ("\"invalid-date\" | \"out-of-range\"", "string"),
        ("request.date[index]", "request.date[10]"),
        ("request.date[index]", "request.date['0']"),
        ("request.date[index]", "(request as any).missing[index]"),
        ("let date = \"\";", "let date = \"\"; request.days='wrong';"),
        (
            "return { ok: false, error: \"invalid-date\" };",
            "return { ok: true, error: 'invalid-date' };",
        ),
        (
            "return { ok: false, error: \"invalid-date\" };",
            "return { ok: false, error: 1 };",
        ),
        (
            "return { ok: false, error: \"invalid-date\" };",
            "return { ok: false, error: 'wrong-error' };",
        ),
        ("code(text,0),code(text,1)", "'bad',code(text,1)"),
        ("code(text,0),code(text,1)", "code(text,0)"),
    ] {
        assert!(
            compile(&source.replace(from, to)).is_err(),
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
