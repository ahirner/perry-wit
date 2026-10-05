use super::instantiate;
use anyhow::Result;

#[tokio::test(flavor = "current_thread")]
async fn rounding_preserves_javascript_ties_and_signed_zero() -> Result<()> {
    let (mut store, instance) = instantiate(
        "export function run(value:number):number {return Math.round(value);}",
        65536,
        |_| Ok(()),
    )
    .await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    for (input, expected) in [
        (2.5, 3.0),
        (-1.5, -1.0),
        (0.5, 1.0),
        (-0.5, -0.0),
        (-0.1, -0.0),
        (-0.0, -0.0),
        (0.0, 0.0),
        (f64::from_bits(0.5f64.to_bits() - 1), 0.0),
        (4503599627370495.5, 4503599627370496.0),
        (4503599627370497.0, 4503599627370497.0),
        (f64::INFINITY, f64::INFINITY),
        (f64::NEG_INFINITY, f64::NEG_INFINITY),
    ] {
        assert_eq!(
            run.call_async(&mut store, (input,)).await?.0.to_bits(),
            expected.to_bits(),
            "{input}"
        );
    }
    assert!(run.call_async(&mut store, (f64::NAN,)).await?.0.is_nan());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn remainder_preserves_fractional_precision_special_values_and_signed_zero() -> Result<()> {
    let (mut store, instance) = instantiate(
        "export function run(left:number,right:number):number {return left%right;}",
        131072,
        |_| Ok(()),
    )
    .await?;
    let run = instance.get_typed_func::<(f64, f64), (f64,)>(&mut store, "run")?;
    for (left, right) in [
        (5.5, 0.1),
        (-5.5, 0.1),
        (5.5, -0.1),
        (-4.0, 2.0),
        (-0.0, 3.0),
        (3.0, f64::INFINITY),
        (-3.0, f64::NEG_INFINITY),
        (f64::MAX, 0.1),
        (f64::MIN_POSITIVE, f64::from_bits(3)),
        (1.0, 0.0),
        (f64::INFINITY, 1.0),
        (f64::NAN, 1.0),
        (1.0, f64::NAN),
    ] {
        let expected = left % right;
        let actual = run.call_async(&mut store, (left, right)).await?.0;
        if expected.is_nan() {
            assert!(actual.is_nan());
        } else {
            assert_eq!(actual.to_bits(), expected.to_bits(), "{left} % {right}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn error_messages_keep_side_effects_and_exception_order() -> Result<()> {
    let source = r#"
    function message(state:Uint8Array):string {state[0]=state[0]+1;return 'message';}
    function failure(state:Uint8Array):string {state[0]=state[0]+1;throw 42;}
    export function run():number {
        const state=new Uint8Array(1);
        try {throw new Error(message(state));} catch {}
        if(state[0]!==1) {throw 98;}
        try {throw new Error(failure(state));} catch(error) {if(error!==42) {throw 99;}}
        return state[0];
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 2.0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn remainder_helpers_share_memory_with_json_and_date_codecs() -> Result<()> {
    let source = r#"
    export function run(divisor:number):number {
        const saved=JSON.parse('{"n":5.5}');
        const date=new Date('1970-01-01');
        let index=0;
        while(index<1000) {
            const encoded=JSON.stringify({value:index%divisor});
            if(encoded===undefined) {throw 99;}
            index=index+1;
        }
        if(date.getTime()!==0) {throw 98;}
        return saved.n%divisor;
    }"#;
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, (0.1,)).await?.0, 5.5 % 0.1);
    Ok(())
}
