use anyhow::Result;
use perry_contract::{Module, Program, generate_wit, resolution, sdk};
use wit_parser::{Resolve, Type, WorldId, WorldItem};

fn generated(source: &str) -> Result<(Resolve, WorldId, String)> {
    let artifacts = generate_wit(
        &Program {
            source: source.as_bytes().into(),
            modules: vec![],
        },
        None,
    )?;
    let text = String::from_utf8(artifacts.into_iter().next().unwrap().contents)?;
    let (resolve, package) = resolution::resolve_embedded_source("world.wit", &text)?;
    let world = resolution::select_world(&resolve, package, Some("app"))?;
    Ok((resolve, world, text))
}

#[test]
fn scalar_defaults_aliases_and_declarations_round_trip() -> Result<()> {
    let source = "import type {u8, s8, u16, s16, u32, s32, u64, s64, f32, f64} from 'perry:types';
        export function numbers(a: number, b: bigint, c: u8, d: s8, e: u16, f: s16, g: u32, h: s32, i: u64, j: s64, k: f32, l: f64): bigint {return b;}";
    let (resolve, world, _) = generated(source)?;
    let function = resolve.worlds[world]
        .exports
        .values()
        .find_map(|item| {
            if let WorldItem::Function(f) = item {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    let types: Vec<_> = function.params.iter().map(|param| param.ty).collect();
    assert_eq!(
        types,
        [
            Type::F64,
            Type::S64,
            Type::U8,
            Type::S8,
            Type::U16,
            Type::S16,
            Type::U32,
            Type::S32,
            Type::U64,
            Type::S64,
            Type::F32,
            Type::F64
        ]
    );
    assert_eq!(function.result, Some(Type::S64));
    let declarations = sdk::declarations(&resolve, world)?;
    let (_, _, regenerated) = generated(std::str::from_utf8(&declarations[0].contents)?)?;
    assert!(
        regenerated.contains("%a: f64, %b: s64, %c: u8, %d: s8"),
        "{regenerated}"
    );
    assert!(
        regenerated.contains("%i: u64, %j: s64, %k: f32, %l: f64"),
        "{regenerated}"
    );
    Ok(())
}

#[test]
fn structures_preserve_presence_and_async_signatures() -> Result<()> {
    let (resolve, world, text) = generated(
        "type Data<T> = { value: T; optional?: string; both: number | null | undefined; nil: bigint | null; bytes: Uint8Array; pair: [boolean, string] }; export async function echo(value: Data<number[]>): Promise<Data<number[]>> {return value;}",
    )?;
    assert!(
        text.contains("perry-undefined-echo-value-optional"),
        "{text}"
    );
    assert!(text.contains("perry-nullish-echo-value-both"), "{text}");
    assert!(text.contains("option<s64>"), "{text}");
    assert!(text.contains("export %echo: async func"), "{text}");
    let declarations = sdk::declarations(&resolve, world)?;
    let (_, _, again) = generated(std::str::from_utf8(&declarations[0].contents)?)?;
    assert!(again.contains("null, undefined, value(f64)"), "{again}");
    assert!(again.contains("undefined, value(string)"), "{again}");
    Ok(())
}

#[test]
fn entry_reexports_share_module_resolution_and_report_source_locations() -> Result<()> {
    let mut input = Program {
        source: b"export { increment as run } from './math.js';".as_slice().into(),
        modules: vec![Module { name: "math.ts".into(), contents: b"import type {u32 as Count} from 'perry:types'; export function increment(n: Count): Count {return n+1;} function helper():void{}".to_vec() }],
    };
    let text = String::from_utf8(generate_wit(&input, Some("custom"))?.remove(0).contents)?;
    assert!(text.contains("world %custom"));
    assert!(text.contains("export %run: func(%n: u32) -> u32"), "{text}");
    assert!(!text.contains("export %helper"));
    input.modules[0].contents =
        b"export function increment(n: unknown): number {return 1;}".to_vec();
    let error = generate_wit(&input, None).unwrap_err();
    let diagnostic = error
        .downcast_ref::<perry_contract::PerryDiagnostic>()
        .unwrap();
    assert_eq!(
        diagnostic.span.as_ref().unwrap().file.to_string_lossy(),
        "math.ts"
    );
    assert!(generate_wit(&input, Some("bad world")).is_err());
    Ok(())
}

#[test]
fn unsupported_public_values_and_ambiguous_types_are_diagnosed() {
    for source in [
        "export const answer = 42;",
        "export default function answer():number{return 42;}",
        "export function maybe(x:boolean){if(x)return 1;}",
        "export function f(x: number | string): void {}",
        "export function f<T>(x:T):T{return x;}",
    ] {
        assert!(generated(source).is_err(), "{source}");
    }
}

#[test]
fn aliases_and_tagged_unions_round_trip_through_declarations() -> Result<()> {
    let (resolve, world, text) = generated(
        "type Choice = 'first' | 'second'; type Ok = {ok:true,value:Choice}; type Err = {ok:false,error:string}; type Outcome = Ok | Err; type Value = {tag:'text',val:string} | {tag:'empty'}; export function run(input: Choice | null | undefined, tagged: Value): Outcome {return {ok:true,value:'first'};}",
    )?;
    assert!(text.contains("result<"), "{text}");
    let declarations = sdk::declarations(&resolve, world)?;
    let (_, _, again) = generated(std::str::from_utf8(&declarations[0].contents)?)?;
    assert!(again.contains("result<"), "{again}");
    assert!(again.contains("null, undefined, value("), "{again}");
    assert!(again.contains("%text(string), %empty"), "{again}");
    Ok(())
}

fn normalized_exports(resolve: &Resolve, world: WorldId) -> Vec<String> {
    fn shape(resolve: &Resolve, ty: Type) -> String {
        use wit_parser::TypeDefKind;
        let Type::Id(id) = ty else {
            return format!("{ty:?}");
        };
        let item = |ty| shape(resolve, ty);
        match &resolve.types[id].kind {
            TypeDefKind::Type(ty) => item(*ty),
            TypeDefKind::List(ty) => format!("list({})", item(*ty)),
            TypeDefKind::Option(ty) => format!("option({})", item(*ty)),
            TypeDefKind::Tuple(tuple) => format!(
                "tuple{:?}",
                tuple.types.iter().map(|ty| item(*ty)).collect::<Vec<_>>()
            ),
            TypeDefKind::Record(record) => format!(
                "record{:?}",
                record
                    .fields
                    .iter()
                    .map(|field| (&field.name, item(field.ty)))
                    .collect::<Vec<_>>()
            ),
            TypeDefKind::Variant(variant) => format!(
                "variant{:?}",
                variant
                    .cases
                    .iter()
                    .map(|case| (&case.name, case.ty.map(item)))
                    .collect::<Vec<_>>()
            ),
            TypeDefKind::Enum(enumeration) => format!(
                "enum{:?}",
                enumeration
                    .cases
                    .iter()
                    .map(|case| &case.name)
                    .collect::<Vec<_>>()
            ),
            TypeDefKind::Result(result) => {
                format!("result{:?}{:?}", result.ok.map(item), result.err.map(item))
            }
            TypeDefKind::Stream(element) => format!("stream{:?}", element.map(item)),
            kind => panic!("unexpected public type {kind:?}"),
        }
    }
    resolve.worlds[world]
        .exports
        .values()
        .filter_map(|item| match item {
            WorldItem::Function(function) => Some(format!(
                "{}:{}:{:?}:{:?}",
                function.name,
                function.kind.is_async(),
                function
                    .params
                    .iter()
                    .map(|p| (&p.name, shape(resolve, p.ty)))
                    .collect::<Vec<_>>(),
                function.result.map(|ty| shape(resolve, ty))
            )),
            WorldItem::Interface { .. } => Some("interface export".into()),
            WorldItem::Type { .. } => None,
        })
        .collect()
}

#[test]
fn supported_contracts_are_equivalent_after_typescript_round_trip() -> Result<()> {
    let source = "import type {f32,u16} from 'perry:types'; type Value = {tag:'text',val:string|undefined} | {tag:'empty'}; type Data = {field?: number; nested: (string|null)[]; pair: [f32,u16]; mode:'on'|'off'; bytes:Uint8Array}; export function run(data:Data,tagged:Value): {ok:true,value:Data}|{ok:false,error:string} {return {ok:true,value:data};}";
    let (resolve, world, _) = generated(source)?;
    let declarations = sdk::declarations(&resolve, world)?;
    let (again, next_world, _) = generated(std::str::from_utf8(&declarations[0].contents)?)?;
    assert_eq!(
        normalized_exports(&resolve, world),
        normalized_exports(&again, next_world)
    );
    Ok(())
}
