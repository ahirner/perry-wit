use super::instantiate;
use crate::waffle_fixture::compile_typescript_waffle;
use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use std::fs;

#[test]
fn temporal_sdk_declares_only_the_supported_immutable_surface() -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let calendar = scratch.path().join("calendar.ts");
    fs::write(
        &calendar,
        format!(
            "type Result<T,E> = T;\n{}",
            include_str!("../fixtures/temporal_shift.ts")
        ),
    )?;
    let utc = scratch.path().join("utc.ts");
    fs::write(
        &utc,
        format!(
            "type Result<T,E> = T;\n{}",
            include_str!("../fixtures/temporal_utc.ts")
        ),
    )?;
    let errors = scratch.path().join("errors.ts");
    fs::write(
        &errors,
        r#"
        const instant=Temporal.Instant.fromEpochMilliseconds(0);
        const plain=Temporal.PlainDateTime.from('2024-01-01');
        const date=Temporal.PlainDate.from('2024-01-01');
        const next:Temporal.PlainDate=date.add({days:1});
        const week:number=date.dayOfWeek+plain.dayOfWeek;
        const text:string=next.toString();
        // @ts-expect-error: immutable property
        date.dayOfWeek=1;
        // @ts-expect-error: no time fields on a plain date
        date.hour;
        // @ts-expect-error: immutable property
        instant.epochMilliseconds=1;
        // @ts-expect-error: immutable property
        plain.year=2000;
        // @ts-expect-error: unsupported constructor
        new Temporal.Instant();
        // @ts-expect-error only ISO string parsing is supported
        Temporal.PlainDateTime.from({year:2024,month:1,day:1});
        // @ts-expect-error only day arithmetic is supported
        plain.add({months:1});
        // @ts-expect-error: no implicit instant conversion
        const wrong:Temporal.Instant=plain;
    "#,
    )?;
    let output = std::process::Command::new("tsc")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sdk/runtime.d.ts"))
        .current_dir(scratch.path())
        .args([
            "--noEmit", "--strict", "--target", "ES2022", "--module", "esnext",
        ])
        .args([calendar, utc, errors])
        .output()?;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn plain_dates_survive_tagged_storage_and_reject_utc_designators() -> Result<()> {
    let source = r#"
    export function run(count:number):Result<string,number> {
        const date=Temporal.PlainDate.from('2024-01-01T23:59:59+03:00');
        if(date.dayOfWeek!==1) {throw 70;}
        const object={date:date};
        if(object.date.dayOfWeek!==1) {throw 71;}
        const array:Temporal.PlainDate[]=[date];
        let index=0;
        while(index<count) {Temporal.PlainDate.from('2000-01-01').toString();index=index+1;}
        if(object.date!==date || array[0]!==date) {throw 99;}
        if(object.date.dayOfWeek!==1) {throw 72;}
        if(array[0].day!==1) {throw 73;}
        let caught=0;
        try {Temporal.PlainDate.from('2024-01-01T00:00Z');} catch {caught=caught+1;}
        try {Temporal.PlainDate.from('2024-01-01T00:00z');} catch {caught=caught+1;}
        if(caught!==2) {throw 97;}
        return object.date.add({days:1}).toString();
    }"#;
    let (mut store, instance) = instantiate(source, 131072, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64,), (Result<String, f64>,)>(&mut store, "run")?;
    for count in [0.0, 1.0, 3000.0] {
        assert_eq!(
            run.call_async(&mut store, (count,)).await?.0,
            Ok("2024-01-02".into()),
            "{count}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn boolean_guards_short_circuit_effects_and_merge_local_assignments() -> Result<()> {
    let source = r#"
    function fail():boolean {throw 99;}
    export function run(input:number):number {
        let seen=false;
        const first=input>0 && (seen=true);
        if(input>0) {if(!first || !seen) {throw 98;}}
        else {if(first || seen) {throw 97;}}
        if(true || fail()) {}
        if(false && fail()) {throw 96;}
        seen=false;
        const second=input>0 || (seen=true);
        if(!second) {throw 95;}
        if(input>0) {if(seen) {throw 94;}} else {if(!seen) {throw 93;}}
        return 1;
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    for input in [-1.0, 0.0, 1.0] {
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, 1.0);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn plain_datetime_source_checks_day_arithmetic_and_codec_errors() -> Result<()> {
    let source = include_str!("../fixtures/temporal_shift.ts");
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run =
        instance.get_typed_func::<(String, f64), (Result<String, f64>,)>(&mut store, "run")?;
    for (input, days, expected) in [
        ("2024-02-28T12:30:00", 1.0, Ok("2024-02-29T12:30:00".into())),
        ("2000-03-01", -1.0, Ok("2000-02-29T00:00:00".into())),
        ("1900-03-01", -1.0, Ok("1900-02-28T00:00:00".into())),
        ("1900-02-29", 0.0, Err(1.0)),
        ("not-a-date", 0.0, Err(1.0)),
        ("2024-01-01", f64::INFINITY, Err(2.0)),
        ("2024-01-01", f64::NAN, Err(2.0)),
        ("2024-01-01", 0.5, Err(2.0)),
    ] {
        assert_eq!(
            run.call_async(&mut store, (input.into(), days)).await?.0,
            expected,
            "{input} + {days}"
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn strict_utc_source_boundary_preserves_precision_and_rejects_other_iso_forms() -> Result<()>
{
    let source = include_str!("../fixtures/temporal_utc.ts");
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(String,), (Result<String, f64>,)>(&mut store, "run")?;
    for input in [
        "1970-01-01T00:00:00.000000001Z",
        "1969-12-31T23:59:59.999999999Z",
        "2024-02-29T12:34:56Z",
    ] {
        assert_eq!(
            run.call_async(&mut store, (input.into(),)).await?.0,
            Ok(input.into())
        );
    }
    for input in [
        "2024-01-01",
        "2024-02-30T00:00:00Z",
        "2024-01-01T00:00:00+00:00",
        "2024-01-01T00:00:60Z",
        "2024-01-01t00:00:00z",
        "2024-01-01T00:00:00.1234567890Z",
        "2024-01-01T00:00:00Z\n",
    ] {
        assert!(
            run.call_async(&mut store, (input.into(),))
                .await?
                .0
                .is_err(),
            "{input}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn temporal_instants_preserve_nanoseconds_offsets_and_numeric_range_errors() -> Result<()> {
    let source = r#"
    function format(value:Temporal.Instant):string {return value.toString();}
    export function run(text:string):Result<string,number> {
        const value=Temporal.Instant.from(text);
        const milliseconds=value.epochMilliseconds;
        const rounded=Temporal.Instant.fromEpochMilliseconds(milliseconds);
        if(rounded.epochMilliseconds!==milliseconds) {throw 99;}
        return format(value);
    }"#;
    let compiled =
        compile_typescript_waffle(source, "instant.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.component_wat.unwrap().contains("wasi:"));
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(String,), (Result<String, f64>,)>(&mut store, "run")?;
    for (input, expected) in [
        (
            "1970-01-01T00:00:00.000000001Z",
            "1970-01-01T00:00:00.000000001Z",
        ),
        (
            "1970-01-01T00:00:00+00:00:00.000000001",
            "1969-12-31T23:59:59.999999999Z",
        ),
        ("2024-02-29T12:00:00+02:00", "2024-02-29T10:00:00Z"),
        ("+275760-09-13T00:00Z", "+275760-09-13T00:00:00Z"),
        ("-271821-04-20T00:00Z", "-271821-04-20T00:00:00Z"),
    ] {
        assert_eq!(
            run.call_async(&mut store, (input.into(),)).await?.0,
            Ok(expected.into())
        );
        store.assert_concurrent_state_empty();
    }
    for input in [
        "2023-02-29T00:00Z",
        "2024-01-01",
        "+275760-09-13T00:00:00.000000001Z",
        "2024-01-01T00:00Z[UTC]",
    ] {
        assert!(
            run.call_async(&mut store, (input.into(),))
                .await?
                .0
                .is_err(),
            "{input}"
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn temporal_plain_day_arithmetic_preserves_fields_and_ignores_numeric_offsets() -> Result<()>
{
    let source = r#"
    function shift(value:Temporal.PlainDateTime, duration:{days:number}):Temporal.PlainDateTime {
        return value.add(duration);
    }
    export function run(text:string, days:number):Result<string,number> {
        const original=Temporal.PlainDateTime.from(text);
        const shifted=shift(original,{days:days});
        if(shifted===original) {throw 99;}
        if(shifted.hour!==original.hour || shifted.minute!==original.minute || shifted.second!==original.second) {throw 98;}
        if(shifted.millisecond!==original.millisecond || shifted.microsecond!==original.microsecond || shifted.nanosecond!==original.nanosecond) {throw 97;}
        return shifted.toString();
    }"#;
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run =
        instance.get_typed_func::<(String, f64), (Result<String, f64>,)>(&mut store, "run")?;
    for (input, days, expected) in [
        (
            "2024-02-28T23:59:58.123456789+05:00",
            2.0,
            "2024-03-01T23:59:58.123456789",
        ),
        ("2000-03-01", -1.0, "2000-02-29T00:00:00"),
        ("1900-03-01", -1.0, "1900-02-28T00:00:00"),
    ] {
        assert_eq!(
            run.call_async(&mut store, (input.into(), days)).await?.0,
            Ok(expected.into())
        );
    }
    for (input, days) in [
        ("2024-01-01T00:00Z", 1.0),
        ("2024-01-01", 0.5),
        ("2024-01-01", f64::NAN),
        ("+275760-09-13", 1.0),
    ] {
        assert!(
            run.call_async(&mut store, (input.into(), days))
                .await?
                .0
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn temporal_values_retain_identity_through_objects_promises_and_collection() -> Result<()> {
    let source = r#"
    async function make(text:string):Promise<Temporal.Instant> {return Temporal.Instant.from(text);}
    function format(value:Temporal.Instant):string {return value.toString();}
    export async function run(text:string):Promise<Result<string,number>> {
        const promise=make(text);
        const value=await promise;
        const owner={instant:value, plain:Temporal.PlainDateTime.from('2024-02-29')};
        let index=0;
        while(index<3000) {
            Temporal.Instant.fromEpochMilliseconds(index).toString();
            Temporal.PlainDateTime.from('2000-01-01').add({days:index}).toString();
            index=index+1;
        }
        if(await promise!==value || owner.instant!==value) {throw 99;}
        if(owner.plain.year!==2024) {throw owner.plain.year;}
        if(owner.plain.month!==2) {throw owner.plain.month;}
        if(owner.plain.day!==29) {throw owner.plain.day;}
        return format(owner.instant);
    }"#;
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(String,), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..10 {
        assert_eq!(
            run.call_async(&mut store, ("1969-12-31T23:59:59.999999999Z".into(),))
                .await?
                .0,
            Ok("1969-12-31T23:59:59.999999999Z".into())
        );
        assert!(
            run.call_async(&mut store, ("bad".into(),))
                .await?
                .0
                .is_err()
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[test]
fn unsupported_temporal_forms_have_compile_time_diagnostics() {
    for expression in [
        "Temporal.Instant.from(0)",
        "Temporal.Instant.from({})",
        "Temporal.Instant.from('2024-01-01T00:00Z',{})",
        "Temporal.Instant.fromEpochMilliseconds('0')",
        "Temporal.Instant.fromEpochNanoseconds(0)",
        "Temporal.PlainDateTime.from({year:2024,month:1,day:1})",
        "Temporal.PlainDateTime.from('2024-01-01').add({months:1})",
        "Temporal.PlainDateTime.from('2024-01-01').add({days:'1'})",
        "Temporal.PlainDateTime.from('2024-01-01').add({days:1,hours:2})",
        "Temporal.Instant.fromEpochMilliseconds(0).toString({timeZone:'UTC'})",
        "Temporal.Instant.fromEpochMilliseconds(0).epochNanoseconds",
        "Temporal.PlainDateTime.from('2024-01-01').toZonedDateTime('UTC')",
        "Temporal.Now.instant()",
        "new Temporal.Instant(0)",
        "Temporal.Instant.from",
        "Temporal.Instant.from(...['1970-01-01T00:00Z'])",
        "Temporal.Instant.fromEpochMilliseconds(0).toString(...[])",
        "Temporal.Instant",
        "Temporal.PlainDateTime.from('2024-01-01').year=2000",
        "(Temporal.Instant.fromEpochMilliseconds(0) as any).epochMilliseconds=1",
        "const wrong:Temporal.Instant=Temporal.PlainDateTime.from('2024-01-01')",
        "let value=Temporal.Instant.fromEpochMilliseconds(0); value=Temporal.PlainDateTime.from('2024-01-01')",
    ] {
        let source = format!("export function run():number {{{expression};return 0;}}");
        assert!(
            compile_typescript_waffle(
                &source,
                "unsupported-time.ts",
                &WaffleCompileOptions::default()
            )
            .is_err(),
            "{expression}"
        );
    }
    for source in [
        "export function run():Temporal.Instant {return Temporal.Instant.fromEpochMilliseconds(0);}",
        "export function run(value:Temporal.Instant):number {return value.epochMilliseconds;}",
        "function from(value:any):Temporal.Instant {return Temporal.Instant.from(value);} export function run():number {return 0;}",
        "function check(value:Temporal.Instant):number {return value.epochMilliseconds;} export function run():number {return check(Temporal.PlainDateTime.from('2024-01-01'));}",
    ] {
        assert!(
            compile_typescript_waffle(source, "time-boundary.ts", &WaffleCompileOptions::default())
                .is_err(),
            "{source}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn temporal_numeric_range_errors_and_returns_survive_allocating_finally() -> Result<()> {
    let source = r#"
    function checked(milliseconds:number,state:Uint8Array):string {
        try {return Temporal.Instant.fromEpochMilliseconds(milliseconds).toString();}
        finally {
            state[0]=state[0]+1;
            let index=0;
            while(index<1000) {Temporal.Instant.fromEpochMilliseconds(index).toString();index=index+1;}
        }
    }
    export function run(milliseconds:number):Result<string,number> {
        const state=new Uint8Array(1);
        try {
            const result=checked(milliseconds,state);
            if(state[0]!==1) {throw 99;}
            return result;
        } catch(error) {
            if(state[0]!==1) {throw 98;}
            throw error;
        }
    }"#;
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64,), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..5 {
        assert_eq!(
            run.call_async(&mut store, (-1.0,)).await?.0,
            Ok("1969-12-31T23:59:59.999Z".into())
        );
        for invalid in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.5,
            8_640_000_000_000_001.0,
        ] {
            assert_eq!(run.call_async(&mut store, (invalid,)).await?.0, Err(2.0));
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}
