use super::*;

#[test]
fn unproductive_alias_expansion_is_bounded() -> Result<()> {
    let ctx = Context::new();
    let mut source = String::from("type Cycle = Cycle; type Fields = {a:number};");
    for index in 0..64 {
        let next = if index == 63 {
            "Fields".into()
        } else {
            format!("Chain{}", index + 1)
        };
        source.push_str(&format!("type Chain{index} = {next};"));
    }
    let ast = crate::hir_compat::parse(&source, "resolution.ts")?;
    let module = perry_hir::lower_module(&ast, "main", "resolution.ts")?;
    for name in ["Cycle", "Chain0"] {
        assert_eq!(
            property_type(
                &ctx,
                &module.interfaces,
                &module.type_aliases,
                name,
                &[],
                "a"
            ),
            None,
            "{name}"
        );
    }
    Ok(())
}
