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
        export function numbers(a: number, b: bigint, c: u8, d: s8, e: u16, f: s16, g: u32, h: s32, i: u64, j: s64, k: f32, l: f64): bigint {return b;}
        function helper(n: bigint) { return n + 1n; }
        export function inferred(n: bigint, flag: boolean) {
            function nested() { return 'ignored'; }
            const arrow = () => true;
            try { if (flag) return helper(n); } finally {}
            return n;
        }";
    let (resolve, world, text) = generated(source)?;
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
    let inferred = perry_contract::world_functions(&resolve, resolve.worlds[world].exports.iter())
        .find(|entry| entry.function.name == "inferred")
        .unwrap();
    assert_eq!(inferred.function.result, Some(Type::S64));
    assert_round_trip(&resolve, world, &text)
}

#[test]
fn structures_preserve_presence_tagged_unions_and_async_signatures() -> Result<()> {
    let (resolve, world, text) = generated(
        "import type {f32,u16} from 'perry:types';
        type Choice = 'first' | 'second';
        type Data<T> = {value:T; optional?:string; both:number|null|undefined; nil:bigint|null; bytes:Uint8Array; pair:[f32,u16]; nested:(string|null)[]};
        type Ok = {ok:true,value:Data<number[]>}; type Err = {ok:false,error:string}; type Outcome = Ok | Err;
        type Tagged = {tag:'text',val:string|undefined} | {tag:'empty'};
        export async function echo(value:Data<number[]>, choice:Choice|null|undefined, tagged:Tagged, stream:ReadableStream<Uint8Array>):Promise<Outcome> {return {ok:true,value};}",
    )?;
    for shape in [
        "null, undefined, value(f64)",
        "undefined, value(string)",
        "option<s64>",
        "result<",
        "stream<u8>",
        "export %echo: async func",
    ] {
        assert!(text.contains(shape), "Missing {shape}: {text}");
    }
    assert_round_trip(&resolve, world, &text)
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
    assert!(generate_wit(&input, Some("bad world")).is_err());
    input.modules[0].contents =
        b"export function increment(n: unknown): number {return 1;}".to_vec();
    let error = generate_wit(&input, None).unwrap_err();
    let diagnostic = error
        .downcast_ref::<perry_wit_diagnostics::PerryDiagnostic>()
        .unwrap();
    assert_eq!(
        diagnostic.span.as_ref().unwrap().file.to_string_lossy(),
        "math.ts"
    );
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
        "export function f(x: {tag?:'a'} | {tag:'b'}):void {}",
        "export function f(x: {tag:'a',tag:'b'} | {tag:'c'}):void {}",
    ] {
        assert!(generated(source).is_err(), "{source}");
    }
}

fn assert_round_trip(resolve: &Resolve, world: WorldId, original: &str) -> Result<()> {
    let declarations = sdk::declarations(resolve, world)?;
    let (_, _, regenerated) = generated(std::str::from_utf8(&declarations[0].contents)?)?;
    assert_eq!(original, regenerated);
    Ok(())
}
