//! Component exports end to end: TypeScript compiled against a WIT world, instantiated with
//! wasmtime's component model, called with WIT values (canonical ABI lift and lower).
use anyhow::Result;
use perry_wit::compile_for_world;
use wasmtime::{
    Engine, ResourceLimiter, Store,
    component::{Component, Linker, Val},
};

/// Records the largest linear memory the guest asks for.
#[derive(Default)]
struct Peak(usize);

impl ResourceLimiter for Peak {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.0 = self.0.max(desired);
        Ok(true)
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

const WIT: &str = r#"
package test:boundary;
world boundary {
  record point { x: f64, y-coord: f64 }
  record summary { count: u32, total: f64, label: string, positive: list<bool> }
  export scale: func(p: point, factor: f64) -> point;
  export describe: func(name: string, values: list<f64>) -> summary;
  export count: func(items: list<string>) -> u32;
  export greet: func(name: string) -> string;
  export negate: func(flag: bool) -> bool;
  export wide: func(a: s64, b: u8, c: f32) -> f64;
  export nested: func(rows: list<list<f64>>) -> list<f64>;
}
"#;

const SOURCE: &str = r#"
interface Point { x: number; yCoord: number }
interface Summary { count: number; total: number; label: string; positive: boolean[] }
export function scale(p: Point, factor: number): Point {
  return { x: p.x * factor, yCoord: p.yCoord * factor };
}
export function describe(name: string, values: number[]): Summary {
  let total = 0;
  const positive: boolean[] = [];
  for (let i = 0; i < values.length; i++) {
    total = total + values[i];
    positive.push(values[i] > 0);
  }
  return { count: values.length, total: total, label: name + "!", positive: positive };
}
export function count(items: string[]): number {
  let n = 0;
  for (let i = 0; i < items.length; i++) {
    n = n + items[i].length;
  }
  return n;
}
export function greet(name: string): string {
  return "héllo " + name;
}
export function negate(flag: boolean): boolean {
  return !flag;
}
export function wide(a: number, b: number, c: number): number {
  return a + b + c;
}
export function nested(rows: number[][]): number[] {
  const sums: number[] = [];
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    let sum = 0;
    for (let j = 0; j < row.length; j++) {
      sum = sum + row[j];
    }
    sums.push(sum);
  }
  return sums;
}
"#;

struct Instance {
    store: Store<Peak>,
    instance: wasmtime::component::Instance,
}

impl Instance {
    fn new() -> Result<Self> {
        let mut resolve = wit_parser::Resolve::default();
        let package = resolve.push_str("boundary.wit", WIT)?;
        let world = resolve.select_world(&[package], Some("boundary"))?;
        let compiled = compile_for_world(SOURCE, "boundary.ts", &resolve, world, true)?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.stripped.unwrap())?;
        let mut store = Store::new(&engine, Peak::default());
        store.limiter(|peak| peak);
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        Ok(Instance { store, instance })
    }

    fn call(&mut self, name: &str, args: &[Val]) -> Result<Val> {
        let function = self
            .instance
            .get_func(&mut self.store, name)
            .unwrap_or_else(|| panic!("missing export {name}"));
        let mut results = [Val::Bool(false)];
        function.call(&mut self.store, args, &mut results)?;
        Ok(results[0].clone())
    }
}

fn record(fields: &[(&str, Val)]) -> Val {
    Val::Record(
        fields
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn completion_results_preserve_payloads_in_sync_and_async_exports() -> Result<()> {
    #[derive(Clone, Copy, Debug)]
    enum Implementation {
        Sync,
        Async,
        Forwarded,
    }
    let label = "é🙂\0".repeat(8_192);
    let bytes = Val::List(vec![Val::U8(255), Val::U8(128)]);
    let text_case = Val::Variant("text".into(), Some(Box::new(Val::String(label.clone()))));
    let bytes_case = Val::Variant("bytes".into(), Some(Box::new(bytes.clone())));
    let bundle = record(&[
        ("a", text_case.clone()),
        ("b", bytes_case.clone()),
        ("c", text_case.clone()),
        ("d", bytes_case.clone()),
        ("e", text_case),
        ("f", bytes_case),
    ]);
    let failure = |name: &str| {
        record(&[
            ("name", Val::String(name.into())),
            ("message", Val::String(label.clone())),
        ])
    };
    let cases = [
        (
            "boolean",
            "result<bool,f64>",
            "boolean",
            "if(fail)throw -0;return false;",
            Some(Val::Bool(false)),
            Some(Val::Float64(-0.0)),
        ),
        (
            "float",
            "result<f32,u32>",
            "number",
            "if(fail)throw 37;return 1.25;",
            Some(Val::Float32(1.25)),
            Some(Val::U32(37)),
        ),
        (
            "padded",
            "result<f32,string>",
            "number",
            "if(fail)throw label;return -1.25;",
            Some(Val::Float32(-1.25)),
            Some(Val::String(label.clone())),
        ),
        (
            "wide-float",
            "result<f32,f64>",
            "number",
            "if(fail)throw -9.5;return 1.25;",
            Some(Val::Float32(1.25)),
            Some(Val::Float64(-9.5)),
        ),
        (
            "narrow-error",
            "result<f64,f32>",
            "number",
            "if(fail)throw -0;return -1.5;",
            Some(Val::Float64(-1.5)),
            Some(Val::Float32(-0.0)),
        ),
        (
            "false-error",
            "result<f64,bool>",
            "number",
            "if(fail)throw false;return -0;",
            Some(Val::Float64(-0.0)),
            Some(Val::Bool(false)),
        ),
        (
            "same-float",
            "result<f64,f64>",
            "number",
            "if(fail)throw -1.25;return Infinity;",
            Some(Val::Float64(f64::INFINITY)),
            Some(Val::Float64(-1.25)),
        ),
        (
            "unit",
            "result",
            "void",
            "if(fail)throw new Error(label);return;",
            None,
            None,
        ),
        (
            "unit-float",
            "result<_,f32>",
            "void",
            "if(fail)throw -1.25;return;",
            None,
            Some(Val::Float32(-1.25)),
        ),
        (
            "text",
            "result<string>",
            "string",
            "if(fail)throw new Error(label);return label;",
            Some(Val::String(label.clone())),
            None,
        ),
        (
            "bytes",
            "result<list<u8>,f64>",
            "Uint8Array",
            "if(fail)throw 9;const bytes=new Uint8Array([0,255,128,42]);return bytes.subarray(1,3);",
            Some(bytes),
            Some(Val::Float64(9.0)),
        ),
        (
            "pack",
            "result<bundle,f64>",
            "Bundle",
            "if(fail)throw 12;const bytes=new Uint8Array([0,255,128,42]).subarray(1,3);return {a:label,b:bytes,c:label,d:bytes,e:label,f:bytes};",
            Some(bundle),
            Some(Val::Float64(12.0)),
        ),
        (
            "legacy",
            "result<f64,f64>",
            "Result<number,number>",
            "if(fail)throw -0;return 1.25;",
            Some(Val::Float64(1.25)),
            Some(Val::Float64(-0.0)),
        ),
        (
            "legacy-unit",
            "result<_,f64>",
            "Result<void,number>",
            "if(fail)throw 12;return;",
            None,
            Some(Val::Float64(12.0)),
        ),
        (
            "error-record",
            "result<string,failure>",
            "string",
            "if(fail){const error=new Error(label);try{throw error;}catch(caught){if(caught!==error)throw 17;throw caught;}}return label;",
            Some(Val::String(label.clone())),
            Some(failure("Error")),
        ),
        (
            "type-error",
            "result<string,failure>",
            "string",
            "if(fail)throw new TypeError(label);return label;",
            Some(Val::String(label.clone())),
            Some(failure("TypeError")),
        ),
        (
            "range-error",
            "result<string,reversed-error>",
            "string",
            "if(fail)throw new RangeError(label);return label;",
            Some(Val::String(label.clone())),
            Some(record(&[
                ("message", Val::String(label.clone())),
                ("name", Val::String("RangeError".into())),
            ])),
        ),
        (
            "syntax-error",
            "result<string,named-error>",
            "string",
            "if(fail)JSON.parse('{');return label;",
            Some(Val::String(label.clone())),
            Some(record(&[("name", Val::String("SyntaxError".into()))])),
        ),
        (
            "message-error",
            "result<string,error-message>",
            "string",
            "if(fail)throw new TypeError(label);return label;",
            Some(Val::String(label.clone())),
            Some(record(&[("message", Val::String(label.clone()))])),
        ),
    ];
    for (wit_async, implementation_kind) in [
        (false, Implementation::Sync),
        (true, Implementation::Sync),
        (false, Implementation::Async),
        (true, Implementation::Async),
        (false, Implementation::Forwarded),
        (true, Implementation::Forwarded),
    ] {
        let keyword = if wit_async { "async " } else { "" };
        let mut wit = String::from(
            "package test:completions; world completions {
                variant payload { text(string), bytes(list<u8>) }
                record bundle { a:payload,b:payload,c:payload,d:payload,e:payload,f:payload }
                record failure { name:string,message:string }
                record reversed-error { message:string,name:string }
                record named-error { name:string }
                record error-message { message:string }",
        );
        let mut source = String::from(
            "type Payload=string|Uint8Array;
             interface Bundle { a:Payload;b:Payload;c:Payload;d:Payload;e:Payload;f:Payload }
             async function detachedFailure():Promise<void>{throw 99;}",
        );
        for (name, wit_type, ts_type, body) in cases
            .iter()
            .map(|(name, wit_type, ts_type, body, _, _)| (*name, *wit_type, *ts_type, *body))
            .chain([
                (
                    "wrong-kind",
                    "result<f64,f64>",
                    "number",
                    "throw new Error(label);",
                ),
                (
                    "unobserved",
                    "result<f64,f64>",
                    "number",
                    "detachedFailure();if(fail)throw 7;return 1;",
                ),
                (
                    "wrong-record",
                    "result<string,failure>",
                    "string",
                    "throw 17;",
                ),
                (
                    "plain",
                    "string",
                    "string",
                    "if(fail)throw new Error(label);return label;",
                ),
                (
                    "discard",
                    "",
                    "number",
                    "if(fail)throw new Error(label);return 9;",
                ),
            ])
        {
            wit.push_str(&format!(
                "export {name}:{keyword}func(fail:bool,label:string)"
            ));
            if !wit_type.is_empty() {
                wit.push_str(&format!("->{wit_type}"));
            }
            wit.push(';');
            let implementation = perry_wit::sdk::codegen::to_camel_case(name);
            match implementation_kind {
                Implementation::Sync => source.push_str(&format!(
                    "export function {implementation}(fail:boolean,label:string):{ts_type}{{{body}}}"
                )),
                Implementation::Async => source.push_str(&format!(
                    "export async function {implementation}(fail:boolean,label:string):Promise<{ts_type}>{{{body}}}"
                )),
                Implementation::Forwarded => source.push_str(&format!(
                    "async function inner{implementation}(fail:boolean,label:string):Promise<{ts_type}>{{{body}}}
                     export function {implementation}(fail:boolean,label:string):Promise<{ts_type}>{{return inner{implementation}(fail,label);}}"
                )),
            }
        }
        wit.push_str(&format!(
            "export before:{keyword}func(fail:bool)->result<f64,f64>;
             export before-plain:{keyword}func(fail:bool,label:string)->f64;"
        ));
        source.push_str("async function readyBefore():Promise<number>{return 7;}
            export function before(fail:boolean):Promise<number>{if(fail)throw -3;return readyBefore();}
            export function beforePlain(fail:boolean,label:string):Promise<number>{return before(fail);}");
        wit.push('}');
        let mut resolve = wit_parser::Resolve::default();
        let package = resolve.push_str("completions.wit", &wit)?;
        let world = resolve.select_world(&[package], Some("completions"))?;
        let compiled = compile_for_world(&source, "completions.ts", &resolve, world, true)?;
        let mut config = wasmtime::Config::new();
        config.wasm_component_model_async(true);
        let engine = Engine::new(&config)?;
        let component = Component::new(&engine, compiled.stripped.unwrap())?;
        let linker = Linker::new(&engine);
        let mut store = Store::new(&engine, Peak::default());
        store.limiter(|peak| peak);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let before =
            instance.get_typed_func::<(bool,), (Result<f64, f64>,)>(&mut store, "before")?;
        let before_plain =
            instance.get_typed_func::<(bool, &str), (f64,)>(&mut store, "before-plain")?;
        let plain = instance.get_typed_func::<(bool, &str), (String,)>(&mut store, "plain")?;
        let discard = instance.get_typed_func::<(bool, &str), ()>(&mut store, "discard")?;
        let mut peak = 0;
        for round in 0..10 {
            for (name, _, _, _, ok, err) in &cases {
                let function = instance.get_func(&mut store, *name).unwrap();
                for (fail, payload) in [(false, ok), (true, err)] {
                    let payload = payload.clone().map(Box::new);
                    let expected = Val::Result(if fail { Err(payload) } else { Ok(payload) });
                    let mut results = [Val::Bool(false)];
                    function
                        .call_async(
                            &mut store,
                            &[Val::Bool(fail), Val::String(label.clone())],
                            &mut results,
                        )
                        .await?;
                    assert_eq!(
                        results[0], expected,
                        "{implementation_kind:?} {keyword}{name} fail={fail}"
                    );
                    if let (Val::Result(actual), Val::Result(expected)) = (&results[0], &expected) {
                        let actual = actual.as_ref().unwrap_or_else(|error| error).as_deref();
                        let expected = expected.as_ref().unwrap_or_else(|error| error).as_deref();
                        match (actual, expected) {
                            (Some(Val::Float64(actual)), Some(Val::Float64(expected))) => {
                                assert_eq!(actual.to_bits(), expected.to_bits(), "{keyword}{name}");
                            }
                            (Some(Val::Float32(actual)), Some(Val::Float32(expected))) => {
                                assert_eq!(actual.to_bits(), expected.to_bits(), "{keyword}{name}");
                            }
                            _ => {}
                        }
                    }
                }
            }
            assert_eq!(before.call_async(&mut store, (false,)).await?.0, Ok(7.0));
            assert_eq!(before.call_async(&mut store, (true,)).await?.0, Err(-3.0));
            assert_eq!(
                before_plain
                    .call_async(&mut store, (false, &label))
                    .await?
                    .0,
                7.0
            );
            assert_eq!(
                plain.call_async(&mut store, (false, &label)).await?.0,
                label
            );
            discard.call_async(&mut store, (false, &label)).await?;
            store.assert_concurrent_state_empty();
            if round == 2 {
                peak = store.data().0;
            } else if round > 2 {
                assert_eq!(
                    store.data().0,
                    peak,
                    "{implementation_kind:?} {keyword}result boundary leaked task storage"
                );
            }
        }
        for (name, fail) in [
            ("wrong-kind", true),
            ("wrong-record", true),
            ("unobserved", false),
            ("unobserved", true),
            ("plain", true),
            ("discard", true),
            ("before-plain", true),
        ] {
            let mut store = Store::new(&engine, Peak::default());
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let function = instance.get_func(&mut store, name).unwrap();
            let mut results = vec![Val::Bool(false); function.ty(&store).results().len()];
            let error = function
                .call_async(
                    &mut store,
                    &[Val::Bool(fail), Val::String(label.clone())],
                    &mut results,
                )
                .await
                .expect_err("uncaught failures without matching WIT error cases must trap");
            assert!(
                matches!(
                    error.downcast_ref::<wasmtime::Trap>(),
                    Some(wasmtime::Trap::UnreachableCodeReached)
                ),
                "{implementation_kind:?} {keyword}{name}: {error:#}"
            );
            drop(store);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_implementations_use_async_wit_without_async_functions() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "sync-source.wit",
        r#"
        package test:sync-source; world boundary {
            export echo:async func(input:string)->string;
            export ping:async func();
            export check:async func(fail:bool)->result<bool,bool>;
        }
    "#,
    )?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    let compiled = compile_for_world(
        r#"
        export function echo(input:string):string{return input;}
        export function ping():void{return;}
        export function check(fail:boolean):boolean{if(fail)throw false;return true;}
    "#,
        "sync-source.ts",
        &resolve,
        world,
        true,
    )?;
    let mut config = wasmtime::Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let echo = instance.get_typed_func::<(&str,), (String,)>(&mut store, "echo")?;
    let ping = instance.get_typed_func::<(), ()>(&mut store, "ping")?;
    let check = instance.get_typed_func::<(bool,), (Result<bool, bool>,)>(&mut store, "check")?;
    let input = "é🙂\0".repeat(8_192);
    let mut peak = 0;
    for round in 0..12 {
        assert_eq!(echo.call_async(&mut store, (&input,)).await?.0, input);
        ping.call_async(&mut store, ()).await?;
        assert_eq!(check.call_async(&mut store, (false,)).await?.0, Ok(true));
        assert_eq!(check.call_async(&mut store, (true,)).await?.0, Err(false));
        store.assert_concurrent_state_empty();
        if round == 2 {
            peak = store.data().0;
        } else if round > 2 {
            assert_eq!(
                store.data().0,
                peak,
                "sync source retained async task memory"
            );
        }
    }
    Ok(())
}

#[test]
fn text_byte_variants_preserve_cases_nested_layouts_and_subviews() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "variants.wit",
        r#"package test:variants; world variants {
            type octet = u8;
            type octets = list<octet>;
            type utf-eight = string;
            variant payload { text(utf-eight), bytes(octets) }
            variant reversed { bytes(octets), text(utf-eight) }
            type alias = payload;
            record bundle { marker: u8, first: alias, second: reversed, items: list<alias>, count: f64 }
            record pair { left: bundle, right: bundle }
            export echo: func(input: alias) -> payload;
            export reverse: func(input: reversed) -> reversed;
            export slice: func(input: payload) -> reversed;
            export text: func(input: string) -> payload;
            export bytes: func(input: octets) -> reversed;
            export pack: func(left: bundle, right: bundle) -> pair;
        }"#,
    )?;
    let world = resolve.select_world(&[package], Some("variants"))?;
    let compiled = compile_for_world(
        r#"
        type Payload = string | Uint8Array;
        interface Bundle { marker:number; first:Payload; second:Payload; items:Payload[]; count:number }
        interface Pair { left:Bundle; right:Bundle }
        export function echo(input:Payload):Payload { return input; }
        export function reverse(input:Uint8Array|string):Payload { return input; }
        export function slice(input:Payload):Payload {
            if(typeof input==='string')return input.slice(1,-1);
            if(input.length>2)input[1]=17;
            return input.subarray(1,-1);
        }
        export function text(input:string):string { return input; }
        export function bytes(input:Uint8Array):Uint8Array { return input.subarray(1); }
        export function pack(left:Bundle,right:Bundle):Pair { return {left:left,right:right}; }
        "#,
        "variants.ts",
        &resolve,
        world,
        true,
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let text = |value: &str| Val::Variant("text".into(), Some(Box::new(Val::String(value.into()))));
    let bytes = |value: &[u8]| {
        Val::Variant(
            "bytes".into(),
            Some(Box::new(Val::List(
                value.iter().copied().map(Val::U8).collect(),
            ))),
        )
    };
    let input = "\u{feff}é🙂\0".repeat(8_192);
    let cases = [
        text(""),
        bytes(&[]),
        text(&input),
        bytes(&[0, 255, 128, 42]),
    ];
    let bundle = record(&[
        ("marker", Val::U8(231)),
        ("first", cases[2].clone()),
        ("second", cases[3].clone()),
        ("items", Val::List(cases.to_vec())),
        ("count", Val::Float64(-7.25)),
    ]);
    let pack = instance.get_func(&mut store, "pack").unwrap();
    let mut results = [Val::Bool(false)];
    let mut peak = 0;
    for round in 0..12 {
        for name in ["echo", "reverse"] {
            let function = instance.get_func(&mut store, name).unwrap();
            for value in &cases {
                function.call(&mut store, std::slice::from_ref(value), &mut results)?;
                assert_eq!(&results[0], value, "{name}");
            }
        }
        pack.call(&mut store, &[bundle.clone(), bundle.clone()], &mut results)?;
        assert_eq!(
            results[0],
            record(&[("left", bundle.clone()), ("right", bundle.clone())])
        );
        if round == 2 {
            peak = store.data().0;
        } else if round > 2 {
            assert_eq!(store.data().0, peak, "variant boundary leaked task storage");
        }
    }
    for (name, input, expected) in [
        ("slice", text("xé🙂y"), text("é🙂")),
        ("slice", bytes(&[0, 255, 128, 42]), bytes(&[17, 128])),
        ("slice", bytes(&[]), bytes(&[])),
        ("text", Val::String("é🙂\0".into()), text("é🙂\0")),
        (
            "bytes",
            Val::List(vec![Val::U8(7), Val::U8(255)]),
            bytes(&[255]),
        ),
    ] {
        instance
            .get_func(&mut store, name)
            .unwrap()
            .call(&mut store, &[input], &mut results)?;
        assert_eq!(results[0], expected, "{name}");
    }
    Ok(())
}

#[test]
fn variant_boundaries_reject_ambiguous_payloads_and_narrow_inputs() -> Result<()> {
    for (variant, source_type, diagnostic) in [
        (
            "text(string), bytes(list<u16>)",
            "string|Uint8Array",
            "exactly one string and one list<u8> case",
        ),
        (
            "first(string), second(string)",
            "string|Uint8Array",
            "exactly one string and one list<u8> case",
        ),
        (
            "text(string), bytes(list<u8>), absent",
            "string|Uint8Array",
            "exactly one string and one list<u8> case",
        ),
        (
            "text(string), bytes(list<u8>)",
            "string",
            "needs TypeScript string | Uint8Array",
        ),
    ] {
        let mut resolve = wit_parser::Resolve::default();
        let package = resolve.push_str("invalid.wit", &format!(
            "package test:invalid; world invalid {{ variant payload {{ {variant} }} export run:func(input:payload)->payload; }}"
        ))?;
        let world = resolve.select_world(&[package], Some("invalid"))?;
        let error = compile_for_world(
            &format!("export function run(input:{source_type}):{source_type}{{return input;}}"),
            "invalid.ts",
            &resolve,
            world,
            true,
        )
        .expect_err("incompatible WIT variant must fail compilation");
        assert!(format!("{error:#}").contains(diagnostic), "{error:#}");
    }
    Ok(())
}

#[test]
fn boolean_outputs_reject_references_and_absent_values() -> Result<()> {
    for asynchronous in [false, true] {
        let keyword = if asynchronous { "async " } else { "" };
        for (wit_type, ts_type, value) in [
            ("bool", "string", "'text'"),
            ("bool", "boolean|undefined", "undefined"),
            ("result<bool,f64>", "string", "'text'"),
            ("result<bool,f64>", "boolean|undefined", "undefined"),
            ("wrapped", "{flag:string}", "{flag:'text'}"),
            ("list<bool>", "string[]", "['text']"),
        ] {
            let mut resolve = wit_parser::Resolve::default();
            let package = resolve.push_str(
                "bool.wit",
                &format!(
                    "package test:bool-output; world boundary {{
                    record wrapped {{ flag:bool }}
                    export run:{keyword}func()->{wit_type};
                }}"
                ),
            )?;
            let world = resolve.select_world(&[package], Some("boundary"))?;
            let ts_type = if asynchronous {
                format!("Promise<{ts_type}>")
            } else {
                ts_type.into()
            };
            let error = compile_for_world(
                &format!("export {keyword}function run():{ts_type}{{return {value};}}"),
                "bool.ts",
                &resolve,
                world,
                true,
            )
            .expect_err("non-boolean output must fail compilation");
            assert!(
                format!("{error:#}").contains("WIT bool needs a TypeScript boolean"),
                "{error:#}"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn single_flat_record_results_keep_their_record_structure() -> Result<()> {
    let cases = [
        (
            "flag",
            "flag-value",
            "Flag",
            record(&[("value", Val::Bool(false))]),
        ),
        (
            "float",
            "float-value",
            "NumberValue",
            record(&[("value", Val::Float32(-1.25))]),
        ),
        (
            "nested",
            "nested-value",
            "Nested",
            record(&[("item", record(&[("value", Val::Float64(-0.0))]))]),
        ),
    ];
    for wit_async in [false, true] {
        for source_async in [false, true] {
            let wit_keyword = if wit_async { "async " } else { "" };
            let source_keyword = if source_async { "async " } else { "" };
            let mut wit = String::from(
                "package test:single-flat; world records {
                 record flag-value { value:bool }
                 record float-value { value:f32 }
                 record number-value { value:f64 }
                 record nested-value { item:number-value }",
            );
            let mut source = String::from(
                "interface Flag { value:boolean }
                 interface NumberValue { value:number }
                 interface Nested { item:NumberValue }",
            );
            for (name, wit_type, ts_type, _) in &cases {
                wit.push_str(&format!(
                    "export {name}:{wit_keyword}func(input:{wit_type})->{wit_type};"
                ));
                let result = if source_async {
                    format!("Promise<{ts_type}>")
                } else {
                    ts_type.to_string()
                };
                source.push_str(&format!(
                    "export {source_keyword}function {name}(input:{ts_type}):{result}{{return input;}}"
                ));
            }
            wit.push_str(&format!(
                "export parsed:{wit_keyword}func(input:string)->flag-value;}}"
            ));
            let result = if source_async {
                "Promise<Flag>"
            } else {
                "Flag"
            };
            source.push_str(&format!(
                "export {source_keyword}function parsed(input:string):{result}{{return JSON.parse(input);}}"
            ));
            let mut resolve = wit_parser::Resolve::default();
            let package = resolve.push_str("single-flat.wit", &wit)?;
            let world = resolve.select_world(&[package], Some("records"))?;
            let compiled = compile_for_world(&source, "single-flat.ts", &resolve, world, true)?;
            let mut config = wasmtime::Config::new();
            config.wasm_component_model_async(true);
            let engine = Engine::new(&config)?;
            let component = Component::new(&engine, compiled.stripped.unwrap())?;
            let mut store = Store::new(&engine, Peak::default());
            store.limiter(|peak| peak);
            let instance = Linker::new(&engine)
                .instantiate_async(&mut store, &component)
                .await?;
            let mut peak = 0;
            for round in 0..10 {
                for (name, _, _, expected) in &cases {
                    let function = instance.get_func(&mut store, *name).unwrap();
                    let mut results = [Val::Bool(false)];
                    function
                        .call_async(&mut store, std::slice::from_ref(expected), &mut results)
                        .await?;
                    assert_eq!(
                        &results[0], expected,
                        "{name}, WIT async={wit_async}, source async={source_async}"
                    );
                    if *name == "nested" {
                        let Val::Record(outer) = &results[0] else {
                            unreachable!()
                        };
                        let Val::Record(inner) = &outer[0].1 else {
                            unreachable!()
                        };
                        let Val::Float64(value) = &inner[0].1 else {
                            unreachable!()
                        };
                        assert_eq!(value.to_bits(), (-0.0f64).to_bits());
                    }
                }
                let parsed = instance.get_func(&mut store, "parsed").unwrap();
                for value in [false, true] {
                    let mut results = [Val::Bool(false)];
                    parsed
                        .call_async(
                            &mut store,
                            &[Val::String(format!("{{\"value\":{value}}}"))],
                            &mut results,
                        )
                        .await?;
                    assert_eq!(results[0], record(&[("value", Val::Bool(value))]));
                }
                store.assert_concurrent_state_empty();
                if round == 2 {
                    peak = store.data().0;
                } else if round > 2 {
                    assert_eq!(
                        store.data().0,
                        peak,
                        "single-flat record retained task storage"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn records_strings_lists_and_scalars_cross_the_boundary() -> Result<()> {
    let mut component = Instance::new()?;
    assert_eq!(
        component.call(
            "scale",
            &[
                record(&[("x", Val::Float64(1.5)), ("y-coord", Val::Float64(-2.0))]),
                Val::Float64(4.0)
            ]
        )?,
        record(&[("x", Val::Float64(6.0)), ("y-coord", Val::Float64(-8.0))])
    );
    assert_eq!(
        component.call(
            "describe",
            &[
                Val::String("sum ✓".into()),
                Val::List(vec![
                    Val::Float64(1.0),
                    Val::Float64(-2.5),
                    Val::Float64(4.0)
                ])
            ]
        )?,
        record(&[
            ("count", Val::U32(3)),
            ("total", Val::Float64(2.5)),
            ("label", Val::String("sum ✓!".into())),
            (
                "positive",
                Val::List(vec![Val::Bool(true), Val::Bool(false), Val::Bool(true)])
            ),
        ])
    );
    // `.length` counts Unicode scalars, including astral characters.
    assert_eq!(
        component.call(
            "count",
            &[Val::List(vec![
                Val::String("ab".into()),
                Val::String("🙂".into()),
                Val::String("é".into()),
                Val::String(String::new()),
            ])]
        )?,
        Val::U32(4)
    );
    assert_eq!(
        component.call("greet", &[Val::String("wörld".into())])?,
        Val::String("héllo wörld".into())
    );
    assert_eq!(
        component.call("negate", &[Val::Bool(true)])?,
        Val::Bool(false)
    );
    assert_eq!(
        component.call("wide", &[Val::S64(-5), Val::U8(200), Val::Float32(0.5)])?,
        Val::Float64(195.5)
    );
    assert_eq!(
        component.call(
            "nested",
            &[Val::List(vec![
                Val::List(vec![Val::Float64(1.0), Val::Float64(2.0)]),
                Val::List(vec![]),
                Val::List(vec![Val::Float64(-3.0)]),
            ])]
        )?,
        Val::List(vec![
            Val::Float64(3.0),
            Val::Float64(0.0),
            Val::Float64(-3.0)
        ])
    );
    Ok(())
}

/// Every call is one task: repeated calls on one instance release their memory.
#[test]
fn repeated_calls_do_not_grow_memory() -> Result<()> {
    let mut component = Instance::new()?;
    let big: Vec<Val> = (0..20_000).map(|i| Val::Float64(f64::from(i))).collect();
    for _ in 0..3 {
        component.call(
            "describe",
            &[Val::String("warm".into()), Val::List(big.clone())],
        )?;
    }
    let before = component.store.data().0;
    for _ in 0..50 {
        component.call(
            "describe",
            &[Val::String("again".into()), Val::List(big.clone())],
        )?;
    }
    assert_eq!(component.store.data().0, before, "memory grew across tasks");
    Ok(())
}

#[test]
fn synchronous_exports_complete_uncontended_async_calls_inline() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "sync-jobs.wit",
        "package test:jobs; world jobs { export run: func(n: u32) -> list<f64>; }",
    )?;
    let world = resolve.select_world(&[package], Some("jobs"))?;
    let source = r#"
async function ready(): Promise<number> { return 0; }
async function append(trace: number[], tag: number): Promise<void> {
  trace.push(tag);
  await ready();
  trace.push(tag+1000);
  await ready();
  trace.push(tag+2000);
}
export function run(n: number): number[] {
  const trace: number[] = [];
  for(let i=0; i<n; i++) append(trace,i);
  trace.push(-1);
  return trace;
}
"#;
    let compiled = compile_for_world(source, "sync-jobs.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), (Vec<f64>,)>(&mut store, "run")?;
    let n = 1000;
    let expected: Vec<f64> = (0..n)
        .flat_map(|tag| [tag, tag + 1000, tag + 2000])
        .chain(std::iter::once(-1))
        .map(f64::from)
        .collect();
    let mut peak = 0;
    for round in 0..10 {
        let (trace,) = run.call(&mut store, (n as u32,))?;
        assert_eq!(trace, expected);
        if round == 0 {
            peak = store.data().0;
        } else {
            assert_eq!(store.data().0, peak, "detached frames leaked task memory");
        }
    }
    Ok(())
}

#[test]
fn byte_views_share_storage_copies_are_independent_and_tasks_release_both() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "bytes.wit",
        "package test:bytes; world bytes { export run: func(input: list<u8>) -> list<u8>; }",
    )?;
    let world = resolve.select_world(&[package], Some("bytes"))?;
    let source = r#"
function verify(input: Uint8Array): Uint8Array {
  const buffer = new ArrayBuffer(input.length + 6);
  const whole = new Uint8Array(buffer);
  const view = new Uint8Array(buffer, 2, input.length);
  const empty = new Uint8Array();
  const literal = Uint8Array.of(257, -1);
  if(literal.length !== 2 || literal[0] !== 1 || literal[1] !== 255) throw 5;
  if(empty.length !== 0 || whole[0] !== 0 || whole[1] !== 0) throw 1;
  if(view.buffer !== buffer || view.byteOffset !== 2 || view.byteLength !== input.length) throw 2;
  view.set(input);
  const copy = new Uint8Array(view);
  const from = Uint8Array.from(view);
  const sliced = view.slice();
  const sub = view.subarray(-3);
  if(sub.buffer !== buffer || copy.buffer === buffer || from.buffer === buffer) throw 3;
  view[0] = 999;
  if(input.length > 0 && (copy[0] !== input[0] || sliced[0] !== input[0] || from[0] !== input[0])) throw 4;
  view.set(copy);
  return whole.subarray(2, input.length + 2);
}
export function run(input: Uint8Array): Uint8Array { return verify(input); }
"#;
    let compiled = compile_for_world(source, "bytes.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&[u8],), (Vec<u8>,)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, (&[],))?.0, Vec::<u8>::new());
    let input: Vec<u8> = (0..50_000).map(|i| i as u8).collect();
    let mut peak = 0;
    for round in 0..20 {
        assert_eq!(run.call(&mut store, (&input,))?.0, input);
        if round == 2 {
            peak = store.data().0;
        }
        if round > 2 {
            assert_eq!(store.data().0, peak, "byte storage leaked across tasks");
        }
    }
    Ok(())
}

#[test]
fn parsed_json_keeps_variants_aliases_and_checked_scalar_boundaries() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "json.wit",
        "package test:json; world json { export run: func(input: string) -> string; }",
    )?;
    let world = resolve.select_world(&[package], Some("json"))?;
    let source = r#"
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
function scalar(value: JsonValue): number {
  if(typeof value !== 'number') throw new TypeError('expected number');
  return value;
}

function array(value: JsonValue): JsonValue[] {
  if(!Array.isArray(value)) throw new TypeError('expected array');
  return value;
}
export function run(input: string): string {
  const parsed = JSON.parse(input);
  if(typeof parsed === 'number') return JSON.stringify(scalar(parsed) + 1);
  if(Array.isArray(parsed)) {
    const items = array(parsed);
    const alias = parsed[0];
    alias.value = 7;
    if(alias !== parsed[0]) throw 1;
    items.push(9);
    return JSON.stringify(parsed);
  }
  return JSON.stringify(parsed);
}
"#;
    let compiled = compile_for_world(source, "json.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for (input, expected) in [
        ("3", "4"),
        ("null", "null"),
        ("true", "true"),
        ("\"é🙂\"", "\"é🙂\""),
        (
            r#"{"b":1,"2":2,"a":3,"b":4,"1":5}"#,
            r#"{"1":5,"2":2,"b":4,"a":3}"#,
        ),
        (r#"[{"value":1}]"#, r#"[{"value":7},9]"#),
    ] {
        assert_eq!(run.call(&mut store, (input,))?.0, expected);
    }
    let deep = format!("{}0{}", "{\"value\":".repeat(127), "}".repeat(127));
    assert_eq!(run.call(&mut store, (&deep,))?.0, deep);
    Ok(())
}

#[test]
fn json_schema_conversion_builds_independent_native_values() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "schema.wit",
        "package test:schema; world schema { export run: func(input: string) -> string; export object-length: func(input: string) -> f64; }",
    )?;
    let world = resolve.select_world(&[package], Some("schema"))?;
    let source = r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {[key:string]:JsonValue};
interface Item { name:string; values:number[] }
export function objectLength(input:string):number {
  try {
    const shape:{length:number}=JSON.parse(input);
    return shape.length;
  } catch(e) { if(e instanceof TypeError)return -1;throw e; }
}
export function run(input:string):string {
  try {
    const parsed=JSON.parse(input);
    const item:Item=parsed;
    const snapshot:JsonValue=item;
    item.values[0]=9;
    item.name='native';
    if(snapshot.name!==parsed.name || snapshot.values[0]!==parsed.values[0])throw 1;
    return JSON.stringify(item);
  } catch(e) { if(e instanceof TypeError)return 'invalid';throw e; }
}
"#;
    let compiled = compile_for_world(source, "schema.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let object_length = instance.get_typed_func::<(&str,), (f64,)>(&mut store, "object-length")?;
    assert_eq!(object_length.call(&mut store, (r#"{"length":7}"#,))?.0, 7.0);
    for input in ["[]", "\"text\"", "null", "{}"] {
        assert_eq!(object_length.call(&mut store, (input,))?.0, -1.0);
    }
    for _ in 0..20 {
        assert_eq!(
            run.call(&mut store, (r#"{"values":[2,3],"name":"json"}"#,))?
                .0,
            r#"{"name":"native","values":[9,3]}"#
        );
        assert_eq!(
            run.call(&mut store, (r#"{"name":"json","values":[false]}"#,))?
                .0,
            "invalid"
        );
        assert_eq!(run.call(&mut store, (r#"{"name":"json"}"#,))?.0, "invalid");
    }
    Ok(())
}
