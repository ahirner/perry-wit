//! Canonical bindings for the functions exposed by the frontend's WIT source view.
use super::bind::abi_type;
use perry_codegen::boundary::Import;
use perry_contract::names::to_camel_case;
use perry_frontend::wit::WorldTypes;
use perry_wit_diagnostics::{Result, codes, diag};
use wit_parser::{Resolve, WorldId};

pub(super) fn bindings(resolve: &Resolve, world: WorldId, types: &mut WorldTypes) -> Vec<Import> {
    let mut imports = Vec::new();
    for entry in perry_contract::world_functions(resolve, resolve.worlds[world].imports.iter()) {
        let module = if entry.interface.is_some() {
            resolve.name_world_key(entry.key)
        } else {
            "$root".into()
        };
        let function = entry.function;
        let binding = format!("{module}#{}", to_camel_case(&function.name));
        let signature = || -> Result<_> {
            let params = function
                .params
                .iter()
                .map(|parameter| abi_type(resolve, &parameter.ty))
                .collect::<Result<_>>()?;
            let result = function
                .result
                .as_ref()
                .map(|ty| abi_type(resolve, ty))
                .transpose()?;
            let source = WorldTypes::function_type(resolve, function).ok_or_else(|| {
                diag!(
                    codes::UNSUPPORTED_TYPE,
                    "WIT function signature has no supported TypeScript representation"
                )
            })?;
            Ok((params, result, source))
        };
        let (params, result, source) = match signature() {
            Ok(parts) => parts,
            Err(reason) => {
                types.import_function(binding, Err(reason));
                continue;
            }
        };
        types.import_function(binding.clone(), Ok(source));
        imports.push(Import {
            binding,
            module,
            name: function.name.clone(),
            params,
            result,
            asynchronous: function.kind.is_async(),
            kind: perry_codegen::boundary::ImportKind::Function,
        });
    }
    imports
}
