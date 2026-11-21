//! Monomorphic parameters of private helpers whose references are all concrete direct calls.
use super::*;

struct Candidate {
    source: hir_compat::PrivateParameters,
    arity: usize,
    types: Option<Vec<HirType>>,
}

pub(crate) fn private_parameters(
    ctx: &Context,
    module: &mut HirModule,
    metadata: &hir_compat::Metadata<'_>,
    mut private_parameters: HashMap<String, hir_compat::PrivateParameters>,
    foreign: &HashMap<String, crate::hir::Foreign>,
) {
    if private_parameters.is_empty() {
        return;
    }
    let mut candidates = HashMap::new();
    for function in module.functions.iter().filter(|f| !f.is_exported) {
        let Some(mut source) = private_parameters.remove(&function.name) else {
            continue;
        };
        source.indices.retain(|&i| {
            function
                .params
                .get(i)
                .is_some_and(|p| matches!(p.ty, HirType::Any | HirType::Unknown))
        });
        if source.indices.is_empty() {
            continue;
        }
        // Only leaf helpers qualify, so a recursive call cannot supply evidence about its own parameter.
        let mut leaf = true;
        hir_compat::body_calls(&function.body, true, &mut |_, _, _| leaf = false);
        if leaf {
            candidates.insert(
                function.id,
                Candidate {
                    source,
                    arity: function.params.len(),
                    types: None,
                },
            );
        }
    }
    if candidates.is_empty() {
        return;
    }
    let decls = shapes::Declarations::new(
        &module.interfaces,
        &module.type_aliases,
        &module.imports,
        &module.classes,
        foreign,
    );
    let inference = Inference {
        ctx,
        decls: &decls,
        metadata,
        generic_functions: None,
    };
    let base = perry_hir::HirTypeEnv::from_module(module);
    let mut locals = HashMap::new();
    let mut observe = |body: &[Stmt], mut env: FunctionEnv<'_>| {
        walk::scan(body, &mut env, None, None);
        hir_compat::body_calls(body, false, &mut |id, offset, args| {
            let Some(candidate) = candidates.get_mut(&id) else {
                return;
            };
            if candidate.arity != args.len() {
                candidates.remove(&id);
                return;
            }
            let actuals = candidate
                .source
                .indices
                .iter()
                .map(|&i| expression(&args[i], &mut env));
            if let Some(types) = &candidate.types {
                if !types
                    .iter()
                    .zip(actuals)
                    .all(|(known, actual)| *known == actual)
                {
                    candidates.remove(&id);
                    return;
                }
            } else {
                let types: Vec<_> = actuals.collect();
                if !types.iter().all(is_resolvable_type) {
                    candidates.remove(&id);
                    return;
                }
                candidate.types = Some(types);
            }
            candidate.source.references.remove(&offset);
        });
    };
    observe(
        &module.init,
        inference.function(&base, &module.classes, &mut locals),
    );
    for function in module
        .functions
        .iter()
        .filter(|f| f.params.iter().all(|p| is_resolvable_type(&p.ty)))
    {
        observe(
            &function.body,
            inference.function_with_params(&base, &module.classes, &mut locals, &function.params),
        );
    }
    for function in &mut module.functions {
        if let Some(candidate) = candidates.remove(&function.id)
            && candidate.source.references.is_empty()
            && let Some(types) = candidate.types
        {
            for (&index, ty) in candidate.source.indices.iter().zip(types) {
                function.params[index].ty = ty;
            }
        }
    }
}
