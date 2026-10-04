use super::instantiate;
use anyhow::Result;
use std::fs;
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

#[tokio::test(flavor = "current_thread")]
async fn options_retain_aliases_mutations_and_unknown_fields_through_helpers_and_collection()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("file"), "😀é")?;
    let source = r#"
    import fs from 'fs';
    interface Options {encoding:string; flag:string;}
    function make(label:string):Options { return {encoding:label,flag:'r'}; }
    function update(options:Options,label:string):Options { options.encoding=label; return options; }
    function text(value:string|Uint8Array):string {
        if (typeof value === 'string') { return value; }
        return new TextDecoder().decode(value);
    }
    export function run(label:string):string {
        const options=make(label);
        const alias=options;
        let index=0;
        while (index<2000) {const garbage={encoding:'utf'+'8', nested:{value:new Uint8Array(128)}}; index=index+1;}
        const first=text(fs.readFileSync('/sandbox/file',options));
        update(alias,'binary');
        if(options.encoding!=='binary') {return 'lost mutation';}
        if (options!==alias) {return 'lost identity';}
        options.extra = new Uint8Array(128);
        try {fs.readFileSync('/outside/file',options);return 'accepted unknown';}
        catch(error) {if(error!==12) {throw error;}}
        delete alias.extra;
        const bytes=fs.readFileSync('/sandbox/file',options);
        options.flag='w';
        fs.writeFileSync('/sandbox/copy',bytes,options);
        options.flag='r';
        delete options.encoding;
        const again=fs.readFileSync('/sandbox/copy',options);
        if(typeof again==='string') {return 'wrong default';}
        return first+new TextDecoder().decode(again);
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    for _ in 0..10 {
        for label in ["UTF-8", "binary"] {
            assert_eq!(
                run.call_async(&mut store, (label.into(),)).await?.0,
                "😀é😀é"
            );
            assert_eq!(fs::read(directory.path().join("copy"))?, "😀é".as_bytes());
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn options_keep_source_order_duplicate_effects_and_rejection_before_io() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("file"), "original")?;
    let source = r#"
    import fs from 'fs';
    type Options={encoding:string;flag:string};
    function effect(state:Uint8Array,text:string):string {state[0]=state[0]+1;return text;}
    function make(state:Uint8Array):Options {
        return {encoding:effect(state,'unsupported'),flag:effect(state,'w'),encoding:effect(state,'utf8'),unknown:effect(state,'effect')};
    }
    export function run(path:string):number {
        const state=new Uint8Array(1);
        const options=make(state);
        try {fs.writeFileSync(effect(state,path),effect(state,'changed'),options);return 0;}
        catch(error) {if(error!==12) {throw error;}}
        if(state[0]!==6) {return -1;}
        delete options.unknown;
        fs.writeFileSync('/sandbox/copy','é😀',options);
        options.flag='r';
        const value=fs.readFileSync('/sandbox/copy',options);
        if(typeof value==='string') {if(value==='é😀') {return state[0];}}
        return -2;
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (f64,)>(&mut store, "run")?;
    for _ in 0..10 {
        for path in ["/sandbox/file", "/sandbox/missing", "/outside/denied"] {
            assert_eq!(run.call_async(&mut store, (path.into(),)).await?.0, 6.0);
            assert_eq!(fs::read(directory.path().join("file"))?, b"original");
            assert!(!directory.path().join("missing").exists());
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retained_options_preserve_nested_values_optional_fields_and_cycles() -> Result<()> {
    let source = r#"
    interface Options {encoding?:string;enabled:boolean;count:number;bytes:Uint8Array;nested:{value:string};}
    type Alias=Options;
    async function retain(value:Alias):Promise<Alias> {return value;}
    function read(value:Alias):string {
        if(value.encoding!==undefined) {return 'unexpected encoding';}
        if(!value.enabled) {return 'lost boolean';}
        if(value.count!==1234.5) {return 'lost number';}
        if(value.bytes[0]!==42) {return 'lost bytes';}
        return value.nested.value;
    }
    export async function run():Promise<string> {
        const value:Alias={enabled:true,count:1234.5,bytes:new Uint8Array([42]),nested:{value:'é'+'😀'}};
        const pending=retain(value);
        let index=0;
        while(index<2000) {
            const garbage={value:new Uint8Array(128)};
            garbage.self=garbage;
            index=index+1;
        }
        const first=await pending;
        if(first!==value) {return 'lost identity';}
        const second=await pending;
        return read(second);
    }"#;
    let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, "é😀");
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stored_metadata_options_validate_current_fields_and_types() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("é😀"), "data")?;
    let source = r#"
    import fs from 'fs';
    interface StatOptions {bigint:boolean;throwIfNoEntry:boolean;}
    interface DirectoryOptions {encoding:string;recursive:boolean;withFileTypes:boolean;}
    function statOptions():StatOptions {return {bigint:false,throwIfNoEntry:true};}
    function directoryOptions():DirectoryOptions {return {encoding:'UTF-8',recursive:false,withFileTypes:false};}
    export function run():number {
        const stat=statOptions();
        const listing=directoryOptions();
        if(stat.bigint) {return -1;}
        if(!stat.throwIfNoEntry) {return -2;}
        if(fs.statSync('/sandbox/é😀',stat).size!==4) {return -3;}
        if(fs.readdirSync('/sandbox',listing)[0]!=='é😀') {return -4;}
        const alias=listing;
        const key='rec'+'ursive';
        alias[key]=true;
        try {fs.readdirSync('/outside',listing);return -5;} catch(error) {if(error!==12) {throw error;}}
        delete alias[key];
        stat.bigint=true;
        try {fs.statSync('/outside',stat);return -6;} catch(error) {if(error!==12) {throw error;}}
        delete stat.bigint;
        listing.unknown={encoding:'utf8'};
        try {fs.readdirSync('/outside',listing);return -7;} catch(error) {if(error!==12) {throw error;}}
        delete listing.unknown;
        return fs.statSync('/sandbox/é😀',stat).size+fs.readdirSync('/sandbox',listing).length;
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 5.0);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn typed_property_reads_reject_missing_or_retagged_fields() -> Result<()> {
    let source = r#"
    interface Options {encoding:string;}
    function read(options:Options):string {return options.encoding;}
    export function run(mode:number):Result<string,number> {
        const options:Options={encoding:'utf8'};
        const key='enc'+'oding';
        if(mode===1) {options[key]=123;}
        if(mode===2) {delete options[key];}
        return read(options);
    }"#;
    let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
    let run = instance.get_typed_func::<(f64,), (Result<String, f64>,)>(&mut store, "run")?;
    for (mode, expected) in [(0.0, Ok("utf8".into())), (1.0, Err(12.0)), (2.0, Err(12.0))] {
        assert_eq!(run.call_async(&mut store, (mode,)).await?.0, expected);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[test]
fn unsupported_objects_are_diagnosed_before_losing_property_effects() {
    use perry_wit::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};
    for declaration in [
        "const options={get encoding() {return 'utf8';}};",
        "const options={set encoding(value:string) {}};",
        "const options={...{encoding:'utf8'}};",
        "const options={['encoding']:'utf8'};",
        "const options={__proto__:{encoding:'utf8'}};",
        "const options={encoding() {return 'utf8';}};",
        "class Options {encoding='utf8';} const options=new Options();",
        "class __AnonShape_fake {encoding='utf8';} const options=new __AnonShape_fake();",
    ] {
        let source = format!(
            "import fs from 'fs'; export function run():number {{{declaration}return fs.readFileSync('/file',options).length;}}"
        );
        assert!(
            compile_typescript_waffle(
                &source,
                "unsupported-options.ts",
                &WaffleCompileOptions::default()
            )
            .is_err(),
            "{declaration}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stored_options_match_node_for_supported_encodings_and_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = r#"
    import fs from 'node:fs';
    interface Options {encoding?:string;flag:string;}
    export function run(path:string):string {
        const options:Options={encoding:'utf8',flag:'w'};
        const alias=options;
        const file=path+'/file';
        fs.writeFileSync(file,'é😀',alias);
        options.flag='r';
        const text=fs.readFileSync(file,options);
        if(typeof text!=='string') {return 'expected text';}
        delete alias.encoding;
        const bytes=fs.readFileSync(file,options.encoding);
        if(typeof bytes==='string') {return 'expected bytes';}
        const metadata={bigint:false,throwIfNoEntry:true};
        if(fs.statSync(file,metadata).size!==bytes.length) {return 'wrong size';}
        fs.unlinkSync(file);
        return text+new TextDecoder().decode(bytes);
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    let actual = run.call_async(&mut store, ("/sandbox".into(),)).await?.0;
    let entry = directory.path().join("compare.mts");
    fs::write(
        &entry,
        format!(
            "{source}\nprocess.stdout.write(run({}));",
            serde_json::to_string(&directory.path())?
        ),
    )?;
    let node = std::process::Command::new("node")
        .arg("--disable-warning=ExperimentalWarning")
        .arg(&entry)
        .output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(actual, "é😀é😀");
    assert_eq!(actual.as_bytes(), node.stdout);
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[test]
fn reusable_options_and_structured_promises_match_sdk_declarations() -> Result<()> {
    let source = r#"
    import fs from 'fs';
    import type {Stats} from 'fs';
    interface ReadOptions {encoding?:string;flag:'r';}
    async function retain(value:Stats):Promise<Stats> {return value;}
    export async function run(path:string):Promise<string[]> {
        const options:ReadOptions={encoding:'utf8',flag:'r'};
        const first:string|Uint8Array=fs.readFileSync(path,options);
        delete options.encoding;
        const second:string|Uint8Array=fs.readFileSync(path,options);
        const stat:Stats=await retain(fs.statSync(path,{bigint:false,throwIfNoEntry:true}));
        if(stat.size<0) {throw 1;}
        fs.writeFileSync(path,first);
        fs.writeFileSync(path,second,{flag:'w'});
        const listing:{encoding?:string;recursive?:false}={encoding:'utf8',recursive:false};
        return fs.readdirSync(path,listing);
    }"#;
    perry_wit::compile_typescript_waffle(
        source,
        "sdk-options.ts",
        &perry_wit::waffle_backend::WaffleCompileOptions::default(),
    )?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("options.ts");
    fs::write(&path, source)?;
    let checked = std::process::Command::new("tsc")
        .current_dir(directory.path())
        .args(["--noEmit", "--strict", "--target", "ES2022"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/types/p3.d.ts"))
        .arg(path)
        .output()?;
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    Ok(())
}
