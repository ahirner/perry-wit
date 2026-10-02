//! Tests for Phase 7: WASI Preview 2 Randomness (`wasi:random`).

mod support;

#[test]
fn math_random_returns_values_in_unit_interval() {
    let source = r#"
        for (let i = 0; i < 5; i++) {
            const r = Math.random();
            console.log(r >= 0.0 && r < 1.0 ? "valid_range" : "invalid");
        }
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "valid_range\nvalid_range\nvalid_range\nvalid_range\nvalid_range\n"
    );
}

#[test]
fn crypto_random_uuid_formats_valid_rfc4122_v4() {
    let source = r#"
        const id1 = crypto.randomUUID();
        const id2 = crypto.randomUUID();
        console.log("len=" + id1.length);
        console.log("hyphen1=" + (id1.charAt(8) === '-'));
        console.log("hyphen2=" + (id1.charAt(13) === '-'));
        console.log("hyphen3=" + (id1.charAt(18) === '-'));
        console.log("hyphen4=" + (id1.charAt(23) === '-'));
        console.log("v4=" + (id1.charAt(14) === '4'));
        const var_char = id1.charAt(19);
        console.log("variant=" + (var_char === '8' || var_char === '9' || var_char === 'a' || var_char === 'b'));
        console.log("diff=" + (id1 !== id2));
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "len=36\nhyphen1=true\nhyphen2=true\nhyphen3=true\nhyphen4=true\nv4=true\nvariant=true\ndiff=true\n"
    );
}

#[test]
fn crypto_get_random_values_mutates_uint8array_in_place_and_returns_array() {
    let source = r#"
        const arr = new Uint8Array(16);
        const ret = crypto.getRandomValues(arr);
        console.log("same_ref=" + (arr === ret));
        console.log("len=" + arr.length);
        let has_nonzero = false;
        for (let i = 0; i < arr.length; i++) {
            if (arr[i] > 0) has_nonzero = true;
        }
        console.log("has_nonzero=" + has_nonzero);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "same_ref=true\nlen=16\nhhas_nonzero=true\n".replace("hhas_nonzero", "has_nonzero")
    );
}

#[test]
fn crypto_get_random_values_on_subarray() {
    let source = r#"
        const orig = new Uint8Array(8);
        orig[0] = 77;
        orig[1] = 88;
        const sub = orig.subarray(2, 8);
        crypto.getRandomValues(sub);
        console.log("head0=" + orig[0]);
        console.log("head1=" + orig[1]);
        let sub_nonzero = false;
        for (let i = 0; i < sub.length; i++) {
            if (sub[i] > 0) sub_nonzero = true;
        }
        console.log("sub_nonzero=" + sub_nonzero);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "head0=77\nhead1=88\nsub_nonzero=true\n"
    );
}

#[test]
fn random_component_prunes_http_and_clocks() {
    let source = r#"
        const uuid = crypto.randomUUID();
        console.log("generated=" + (uuid.length === 36));
    "#;
    let output = support::run(source, None, None);
    assert_eq!(support::stdout(&output).trim(), "generated=true");

    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    assert!(
        wat.contains("wasi:random"),
        "random component should import wasi:random"
    );
    assert!(
        !wat.contains("wasi:http"),
        "random component should not import wasi:http"
    );
    assert!(
        !wat.contains("wasi:clocks"),
        "random component should not import wasi:clocks"
    );
}
