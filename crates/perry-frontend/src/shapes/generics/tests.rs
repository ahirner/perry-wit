use super::*;

#[test]
fn unproductive_alias_expansion_is_bounded() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let mut source =
        String::from("type Cycle = Cycle; type Fields = {a:number}; interface Base { a:number }");
    for index in 0..64 {
        let next = if index == 63 {
            "Fields".into()
        } else {
            format!("Chain{}", index + 1)
        };
        let parent = if index == 0 {
            "Base".into()
        } else {
            format!("Parent{}", index - 1)
        };
        source.push_str(&format!(
            "type Chain{index} = {next}; interface Parent{index} extends {parent} {{}}"
        ));
    }
    let ast = crate::source::parse(&source, "resolution.ts")?;
    let module = perry_hir::lower_module(&ast, "main", "resolution.ts")?;
    let ctx = Context::new();
    let foreign = crate::wit::WorldTypes::default();
    let decls = Declarations::new(
        &ctx,
        &module.interfaces,
        &module.type_aliases,
        &module.imports,
        &module.classes,
        &foreign,
        &crate::hir_compat::Locations::default(),
    )?;
    let mut types = TypeTable::new();
    let mut resolver = Resolver::new(&ctx, &decls, &mut types);
    let mut field = |name: &str| {
        let ty = resolver.types.named(&ctx, name);
        resolver.property(ty, "a")
    };
    for name in ["Cycle", "Chain0"] {
        assert_eq!(field(name), None, "{name}");
    }
    // Warm the parents in order; a deep query has the same limit with a warm cache.
    for index in 0..64 {
        let field = field(&format!("Parent{index}"));
        if index == 63 {
            assert_eq!(field, None);
        }
    }
    Ok(())
}
