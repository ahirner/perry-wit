use super::{Host, instantiate};
use anyhow::Result;
use wasmtime::StoreContextMut;

#[tokio::test(flavor = "current_thread")]
async fn dynamic_values_preserve_types_equality_and_checked_boundaries() -> Result<()> {
    let source = r#"
    function identity(value:any):any {return value;}
    function typed(value:Date):number {return value.getTime();}
    function unbox(value:any):Date {return value;}
    function truth(value:any):boolean {if(value) {return true;} return false;}
    function arithmetic(value:any):number {return value*2;}
    export function run():number {
        if(identity(null)!==null) {throw 1;}
        if(identity(undefined)!==undefined) {throw 2;}
        if(identity(false)!==false) {throw 3;}
        if(identity(0)!==0) {throw 4;}
        if(identity(0)===false) {throw 5;}
        if(identity(0/0)===identity(0/0)) {throw 6;}
        if(identity(-0)!==identity(0)) {throw 7;}
        if(identity('😀é')!==identity('😀'+'é')) {throw 8;}
        if(typeof identity(undefined)!=='undefined') {throw 9;}
        if(typeof identity(null)!=='object') {throw 10;}
        if(typeof identity(false)!=='boolean') {throw 11;}
        if(typeof identity(0/0)!=='number') {throw 12;}
        if(typeof identity('😀')!=='string') {throw 13;}
        const date=new Date(-1);
        if(identity(date)!==date) {throw 14;}
        if(identity(new Date(-1))===date) {throw 15;}
        if(typed(identity(date))!==-1) {throw 16;}
        if(unbox(identity(date))!==date) {throw 17;}
        const object={value:'kept'};
        if(identity(object)!==object) {throw 18;}
        const bytes=new Uint8Array([1,2,3]);
        if(identity(bytes)!==bytes) {throw 19;}
        if(truth(undefined)) {throw 20;}
        if(truth(null)) {throw 21;}
        if(truth(false)) {throw 22;}
        if(truth(0)) {throw 23;}
        if(truth(0/0)) {throw 24;}
        if(truth('')) {throw 25;}
        if(!truth('😀')) {throw 26;}
        if(!truth(date)) {throw 27;}
        if(!truth(object)) {throw 28;}
        if(arithmetic(null)!==0) {throw 29;}
        if(arithmetic(true)!==2) {throw 30;}
        try {typed(identity(1));throw 31;} catch(error) {if(error!==12) {throw error;}}
        return 42;
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 42.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_missing_text_and_byte_values_preserve_undefined() -> Result<()> {
    let source = r#"
    function identity(value:any):any {return value;}
    function discard(value:any):void {return value;}
    export function run():number {
        const empty=new Uint8Array(0);
        for(let index=0;index<4000;index=index+1) {
            if(identity(''[0])!==undefined) {throw 1;}
            if(identity('é'[0])!=='é') {throw 2;}
            if(identity(''.codePointAt(0))!==undefined) {throw 3;}
            if(identity(empty[0])!==undefined) {throw 4;}
            if(identity('😀'.codePointAt(0))!==128512) {throw 5;}
            discard(identity(''.codePointAt(0)));
        }
        return 42;
    }"#;
    let (mut store, instance) = instantiate(source, 512 * 1024, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 42.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn mixed_fields_aliases_and_retained_outcomes_keep_their_references() -> Result<()> {
    let source = r#"
    function identity(value:any):any {return value;}
    function time(value:any):number {return new Date(value).getTime();}
    async function retain(value:any):Promise<any> {return value;}
    export async function run():Promise<number> {
        const date=new Date(-1);
        const state:{value:any}={value:date};
        const alias=state;
        const pending=retain(state.value);
        let value=identity(date);
        value=false;
        if(time(value)!==0) {throw 98;}
        let index=0;
        while(index<4000) {
            identity(new Date(index));
            state.value=index;
            state.value=identity(date);
            index=index+1;
        }
        if(alias.value!==date) {throw 97;}
        if(await pending!==date) {throw 96;}
        if(await pending!==await pending) {throw 95;}
        const captured=await pending;
        delete state.value;
        const optional:{value?:any}=state;
        if(optional.value!==undefined) {throw 94;}
        try {return time(captured);}
        finally {let cleanup=0;while(cleanup<4000) {identity('é'+'😀');cleanup=cleanup+1;}}
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..10 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, -1.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_returns_preserve_undefined_and_adopt_typed_task_outcomes() -> Result<()> {
    let source = r#"
    function empty():any {}
    function bare():any {return;}
    async function asyncEmpty():Promise<any> {}
    async function asyncBare():Promise<any> {return;}
    async function date():Promise<Date> {return new Date(-1);}
    async function number():Promise<number> {return 42;}
    async function nothing():Promise<void> {}
    async function widened():Promise<any> {return date();}
    async function widenedNumber():Promise<any> {return number();}
    async function widenedVoid():Promise<any> {return nothing();}
    async function narrowed():Promise<Date> {return widened();}
    function identity(value:any):any {return value;}
    async function opaque(value:any):Promise<any> {return value;}
    export async function run():Promise<number> {
        if(empty()!==undefined) {throw 99;}
        if(bare()!==undefined) {throw 98;}
        if(await asyncEmpty()!==undefined) {throw 97;}
        if(await asyncBare()!==undefined) {throw 96;}
        if(await widenedVoid()!==undefined) {throw 95;}
        if(await widenedNumber()!==42) {throw 94;}
        const pending=widened();
        const value=await pending;
        if(await pending!==value) {throw 93;}
        if(new Date(value).getTime()!==-1) {throw 92;}
        const concrete=await narrowed();
        if(concrete.getTime()!==-1) {throw 91;}
        const primitive=await identity(null);
        if(primitive!==null) {throw 90;}
        const original=date();
        try {await opaque(original);throw 89;}
        catch(error) {if(error!==12) {throw error;}}
        try {await identity(original);throw 88;}
        catch(error) {if(error!==12) {throw error;}}
        const settled=await original;
        return settled.getTime();
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, -1.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn existing_numeric_any_component_boundaries_box_and_validate_results() -> Result<()> {
    for async_entry in [false, true] {
        let source = if async_entry {
            r#"
        async function identity(value:any):Promise<any> {return value;}
        export async function run(value:any,fail:boolean):Promise<Result<any,number>> {
            const pending=identity(value);
            if(fail) {return 'wrong result type';}
            return await pending;
        }"#
        } else {
            r#"
        export function run(value:any,fail:boolean):Result<any,number> {
            if(fail) {return 'wrong result type';}
            return value;
        }"#
        };
        let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
        let run = instance.get_typed_func::<(f64, bool), (Result<f64, f64>,)>(&mut store, "run")?;
        for _ in 0..20 {
            assert_eq!(run.call_async(&mut store, (42.5, false)).await?.0, Ok(42.5));
            assert_eq!(run.call_async(&mut store, (42.5, true)).await?.0, Err(12.0));
            assert!(
                run.call_async(&mut store, (f64::NAN, false))
                    .await?
                    .0
                    .unwrap()
                    .is_nan()
            );
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_random_destinations_validate_runtime_tags_before_host_calls() -> Result<()> {
    let source = r#"
    function fill(value:any):Uint8Array {return crypto.getRandomValues(value);}
    export function run():number {
        for(let index=0;index<1000;index=index+1) {
            try {fill(0/0);throw 99;} catch(error) {if(error!==1) {throw error;}}
            try {fill('bad');throw 98;} catch(error) {if(error!==1) {throw error;}}
            const bytes=new Uint8Array([7,0,0,9]);
            const view=bytes.subarray(1,3);
            if(fill(view)!==view) {throw 97;}
            if(bytes[1]!==165) {throw 96;}
            if(bytes[2]!==165) {throw 95;}
            if(bytes[0]!==7) {throw 94;}
            if(bytes[3]!==9) {throw 93;}
        }
        return 42;
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |linker| {
        linker.instance("wasi:random/random@0.3.0")?.func_wrap(
            "get-random-bytes",
            |_: StoreContextMut<'_, Host>, (length,): (u64,)| {
                assert_eq!(length, 2);
                Ok((vec![165u8; 2],))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 42.0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn boxed_arguments_and_numeric_errors_survive_later_argument_effects_and_cleanup()
-> Result<()> {
    let source = r#"
    function identity(value:any):any {return value;}
    function churn():number {
        let index=0;
        while(index<4000) {identity(new Date(index));index=index+1;}
        return 2;
    }
    function choose(first:any,second:number):number {return new Date(first).getTime()+second;}
    function raise(value:any):number {throw value;}
    function invalid(value:any):number {return new Date(value).getTime();}
    export function run():Result<number,number> {
        if(choose(new Date(-1),churn())!==1) {throw 99;}
        try {invalid('unsupported parsing');throw 98;}
        catch(error) {if(error!==12) {throw error;}}
        try {return raise(identity(7));} finally {churn();}
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (Result<f64, f64>,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, Err(7.0));
    }
    Ok(())
}
