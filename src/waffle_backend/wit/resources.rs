//! Opaque resource bindings and the canonical operations owned by each interface.

use super::*;
use wit_parser::{Handle, TypeId, TypeOwner};

pub(super) const STATE: &str = "__perry_resource_state";
pub(super) fn key(id: TypeId) -> String {
    format!("__perry_resource_{}", id.index())
}
pub(super) fn handle_id(resolve: &Resolve, handle: Handle) -> TypeId {
    let mut id = match handle {
        Handle::Own(id) | Handle::Borrow(id) => id,
    };
    while let TypeDefKind::Type(Type::Id(inner)) = resolve.types[id].kind {
        id = inner;
    }
    id
}
pub(super) fn hir_type(id: TypeId) -> HirType {
    record([(key(id), HirType::Number), (STATE.into(), HirType::Number)])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::waffle_backend) enum Operation {
    New,
    Rep,
    Drop,
}
#[derive(Clone, Debug)]
pub(in crate::waffle_backend) struct Resource {
    pub module: String,
    pub name: String,
    pub exported: bool,
}
#[derive(Clone, Copy)]
pub(crate) struct Functions {
    pub new: Option<waffle::Func>,
    pub rep: Option<waffle::Func>,
    pub drop: waffle::Func,
}

pub(super) fn bindings(
    resolve: &mut Resolve,
    world: WorldId,
    imports: &mut BTreeMap<String, WitImport>,
) -> Result<BTreeMap<TypeId, Resource>> {
    let mut resources = BTreeMap::new();
    for (exported, (key, item)) in resolve.worlds[world]
        .imports
        .iter()
        .map(|v| (false, v))
        .chain(resolve.worlds[world].exports.iter().map(|v| (true, v)))
    {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let module = resolve.name_world_key(key);
        for (name, ty) in &resolve.interfaces[*id].types {
            if matches!(resolve.types[*ty].kind, TypeDefKind::Resource) {
                ensure!(
                    !resources.contains_key(ty),
                    "A resource cannot be both imported and exported"
                );
                resources.insert(
                    *ty,
                    Resource {
                        module: module.clone(),
                        name: name.clone(),
                        exported,
                    },
                );
            }
        }
    }
    for (id, resource) in &resources {
        let mut own = resolve.types[*id].clone();
        own.name = None;
        own.owner = TypeOwner::None;
        own.kind = TypeDefKind::Handle(Handle::Own(*id));
        let own = Type::Id(resolve.types.alloc(own));
        let mut borrow = resolve.types[*id].clone();
        borrow.name = None;
        borrow.owner = TypeOwner::None;
        borrow.kind = TypeDefKind::Handle(Handle::Borrow(*id));
        let borrow = Type::Id(resolve.types.alloc(borrow));
        let operations = if resource.exported {
            &[Operation::New, Operation::Rep, Operation::Drop][..]
        } else {
            &[Operation::Drop][..]
        };
        for op in operations {
            let (name, param, result) = match op {
                Operation::New => (format!("new-{}", resource.name), Type::U32, Some(own)),
                Operation::Rep => (format!("{}-rep", resource.name), borrow, Some(Type::U32)),
                Operation::Drop => (format!("drop-{}", resource.name), own, None),
            };
            let function = Function {
                name: name.clone(),
                kind: FunctionKind::Freestanding,
                params: vec![wit_parser::Param {
                    name: "value".into(),
                    ty: param,
                    span: Default::default(),
                }],
                result,
                docs: Default::default(),
                stability: Default::default(),
                span: Default::default(),
                external_id: None,
            };
            let key = format!("{}#{name}", resource.module);
            ensure!(
                !imports.contains_key(&key),
                "Resource helper name conflicts with WIT function '{name}'"
            );
            imports.insert(
                key,
                WitImport {
                    module: resource.module.clone(),
                    function,
                    resource: Some((*id, *op)),
                },
            );
        }
    }
    Ok(resources)
}

pub(in crate::waffle_backend) fn declare(
    module: &mut waffle::Module<'static>,
    contract: &crate::waffle_backend::resolve::ResolvedContract,
) -> BTreeMap<TypeId, Functions> {
    let mut used = BTreeSet::new();
    let Some(wit) = &contract.wit else {
        return BTreeMap::new();
    };
    fn collect(resolve: &Resolve, ty: Type, used: &mut BTreeSet<TypeId>) {
        let Type::Id(id) = ty else { return };
        match &resolve.types[id].kind {
            TypeDefKind::Handle(h) => {
                used.insert(handle_id(resolve, *h));
            }
            TypeDefKind::Type(t) | TypeDefKind::List(t) | TypeDefKind::Option(t) => {
                collect(resolve, *t, used)
            }
            TypeDefKind::Record(r) => {
                for f in &r.fields {
                    collect(resolve, f.ty, used)
                }
            }
            TypeDefKind::Tuple(t) => {
                for t in &t.types {
                    collect(resolve, *t, used)
                }
            }
            TypeDefKind::Variant(v) => {
                for c in &v.cases {
                    if let Some(t) = c.ty {
                        collect(resolve, t, used)
                    }
                }
            }
            TypeDefKind::Result(r) => {
                for t in r.ok.iter().chain(&r.err) {
                    collect(resolve, *t, used)
                }
            }
            _ => {}
        }
    }
    for function in
        wit.functions
            .values()
            .map(|e| &e.function)
            .chain(contract.intrinsics.values().filter_map(|intrinsic| {
                if let crate::waffle_backend::resolve::TypedIntrinsic::WitImport { key, .. } =
                    intrinsic
                {
                    Some(&wit.imports[key].function)
                } else {
                    None
                }
            }))
    {
        for param in &function.params {
            collect(&wit.resolve, param.ty, &mut used)
        }
        if let Some(result) = function.result {
            collect(&wit.resolve, result, &mut used)
        }
    }
    let mut functions = BTreeMap::new();
    for id in used {
        let resource = &wit.resources[&id];
        let mut declare = |op: &str, returns: Vec<CoreType>| {
            let signature = module.signatures.push(SignatureData {
                params: vec![CoreType::I32],
                returns,
            });
            let name = format!("[resource-{op}]{}", resource.name);
            let function = module
                .funcs
                .push(waffle::FuncDecl::Import(signature, name.clone()));
            module.imports.push(waffle::Import {
                module: format!(
                    "{}{}",
                    if resource.exported { "[export]" } else { "" },
                    resource.module
                ),
                name,
                kind: waffle::ImportKind::Func(function),
            });
            function
        };
        let drop = declare("drop", vec![]);
        let new = resource
            .exported
            .then(|| declare("new", vec![CoreType::I32]));
        let rep = resource
            .exported
            .then(|| declare("rep", vec![CoreType::I32]));
        functions.insert(id, Functions { new, rep, drop });
    }
    functions
}
