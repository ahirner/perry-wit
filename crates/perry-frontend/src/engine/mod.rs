//! Data-oriented monomorphization and type-inference engine over dense indices.
pub(crate) mod bitset;
pub(crate) mod boundary;
pub(crate) mod builtins;
mod instances;
mod plans;
pub(crate) mod solve;
pub(crate) mod types;

use crate::source::SourceModule;
use boundary::ModuleIndex;
use perry_wit_diagnostics::Result;
pub(crate) use plans::BodyOwner;
use pliron::context::Context;
use std::collections::HashMap;
use types::TypeTable;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellInit {
    AtDeclaration,
    Deferred,
    Hoisted,
}

impl CellInit {
    pub(crate) fn admits_undefined(self) -> bool {
        self != Self::AtDeclaration
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Storage {
    ErasedFunction(Option<perry_hir::types::FuncId>),
    Value(types::Ty),
    Boxed { ty: types::Ty, init: CellInit },
    Promise(types::Ty),
}

#[derive(Clone, Copy)]
pub(crate) enum InstanceResult {
    Source,
    Value(types::Ty),
    ErasedFunction,
}

pub(crate) struct InstanceSignature {
    pub(crate) inline: Option<solve::Invocation>,
    pub(crate) environment: Option<types::FuncIdentity>,
    pub(crate) receiver: Option<types::Ty>,
    pub(crate) result: InstanceResult,
}

pub(crate) struct DispatchArm {
    pub(crate) identity: types::FuncIdentity,
    pub(crate) target: perry_hir::types::FuncId,
    pub(crate) receiver: bool,
    pub(crate) arguments: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum NativeParam {
    Value(types::Ty),
    Environment(types::FuncIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TimerKind {
    Immediate,
    Timeout,
    Interval,
}

pub(crate) enum Reaction {
    Chain {
        fulfilled: Option<solve::CallTarget>,
        rejected: Option<solve::CallTarget>,
        rejects_only: bool,
    },
    Finally(solve::CallTarget),
}

pub(crate) enum NativeBody {
    Resource(crate::hir_compat::Target<perry_hir::types::FuncId>),
    Dispatch {
        receiver: bool,
        arms: Vec<DispatchArm>,
    },
    Settle(perry_mir::SettleKind),
    Reaction {
        reaction: Reaction,
        returns_void: bool,
    },
    Timer {
        kind: TimerKind,
        callback: solve::CallTarget,
    },
}

pub(crate) struct NativeFunction {
    pub(crate) name: String,
    pub(crate) params: Vec<NativeParam>,
    pub(crate) result: types::Ty,
    pub(crate) is_async: bool,
    pub(crate) body: NativeBody,
}

#[derive(Clone, Copy)]
pub(crate) enum EnvironmentField {
    Capture {
        local: perry_hir::types::LocalId,
        ty: types::Ty,
        cell: Option<CellInit>,
    },
    Settler(types::Ty),
    Unit,
}

impl EnvironmentField {
    pub(crate) fn storage(&self) -> Storage {
        match *self {
            Self::Capture { ty, cell, .. } => {
                cell.map_or(Storage::Value(ty), |init| Storage::Boxed { ty, init })
            }
            Self::Settler(ty) => Storage::Promise(ty),
            Self::Unit => Storage::Value(types::Ty::NUMBER),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CaptureOperand {
    Local(perry_hir::types::LocalId),
    Cell(perry_hir::types::LocalId),
    Coerce(perry_hir::types::LocalId, solve::ValueCoercion),
}

pub(crate) struct ClosureValue {
    pub(crate) identity: types::FuncIdentity,
    pub(crate) operands: Vec<CaptureOperand>,
}

#[derive(Default)]
pub(crate) struct InstancePlans {
    decisions: plans::Decisions,
    functions: types::IdxVec<types::InstanceId, perry_hir::types::FuncId>,
    helpers: types::IdxVec<types::HelperId, Option<perry_hir::types::FuncId>>,
    pub(super) local_ids: Vec<Option<types::LocalIdx>>,
    local_flags: types::IdxVec<types::LocalIdx, types::LocalFlags>,
    pub(crate) closure_values: HashMap<(BodyOwner, perry_hir::types::FuncId), ClosureValue>,
    pub(crate) environments: plans::Environments,
}

pub(crate) struct Specialization<'s> {
    pub(crate) iteration: crate::hir_compat::Iteration<'s>,
    pub(crate) literal_classes: crate::source::LiteralClasses<'s>,
    pub(crate) annotations: boundary::Annotations,
    pub(crate) lexical: boundary::LexicalFunctions<'s>,
    pub(crate) receiver_members: crate::wit::ReceiverMembers<'s>,
    pub(crate) regexes: crate::source::RegexSites<'s>,
    pub(crate) bodies: crate::source::InstanceBodies<'s>,
    pub(crate) sparse_arrays: bool,
    pub(crate) locations: types::IdxVec<types::LocId, types::Ty>,
    pub(crate) dict_reads: bitset::BitSet,
    pub(crate) capture_free: bitset::BitSet,
    pub(crate) escaping_env_templates: bitset::BitSet,
    pub(crate) instances: InstancePlans,
    pub(crate) resources:
        std::collections::BTreeMap<u32, crate::hir_compat::ResourceClass<types::ClassInstId>>,
}

pub(crate) fn lower_module(
    ctx: &mut Context,
    mut source: SourceModule<'_>,
    world: &crate::wit::WorldTypes<'_>,
) -> Result<crate::LoweredProgram> {
    let mut types = TypeTable::new();
    let decls = crate::shapes::Declarations::new(
        &source.declarations.interfaces,
        &source.declarations.type_aliases,
        &source.declarations.imports,
        &source.bodies.classes,
        world,
    );
    let mut index = ModuleIndex::build(
        ctx,
        &source.bodies,
        &source.declarations,
        &source.locations,
        &source.closure_display_names,
        (&source.constructors, &decls),
        &mut types,
    );
    let mut solved = solve::solve(ctx, &source, &index, &decls, &mut types)?;
    let mut records = perry_mir::RecordLayouts::default();
    let (bodies, mut instances, decisions) = instances::build(
        &source.bodies,
        &source.declarations,
        &mut source.locations,
        &index,
        &types,
        &solved,
        (ctx, &mut records),
    )?;
    instances.decisions = plans::Decisions::from_solver(&mut index, &mut solved, decisions);
    instances.local_ids = index.local_to_idx;
    instances.local_flags = index.locals.map(|local| local.flags);
    let capture_free = index.capture_free_templates;
    let specialization = Specialization {
        resources: source
            .exports
            .resources
            .into_iter()
            .map(|(id, resource)| {
                let class = *solved.resource_instances.get(&id).ok_or_else(|| {
                    perry_wit_diagnostics::internal!("missing resource specialization")
                })?;
                Ok((
                    id,
                    crate::hir_compat::ResourceClass {
                        class,
                        binding: resource.binding,
                    },
                ))
            })
            .collect::<Result<_>>()?,
        iteration: index.iteration,
        literal_classes: index.literal_classes,
        annotations: index.annotations,
        lexical: index.functions,
        regexes: index.regexes,
        receiver_members: index.receiver_members,
        bodies,
        sparse_arrays: source.sparse_arrays || solved.deletes_array_elements,
        locations: solved.locations,
        dict_reads: solved.dict_reads,
        capture_free,
        escaping_env_templates: solved.escaping_env_templates,
        instances,
    };
    crate::lower::program::build_module(
        ctx,
        &source.declarations,
        decls,
        &mut source.locations,
        &source.exports.functions,
        &specialization,
        (types, records),
    )
}

impl InstancePlans {
    pub(crate) fn function(&self, id: types::InstanceId) -> perry_hir::types::FuncId {
        self.functions[id]
    }

    pub(crate) fn helper(&self, id: types::HelperId) -> Result<perry_hir::types::FuncId> {
        self.helpers[id]
            .ok_or_else(|| perry_wit_diagnostics::Diagnostic::internal("missing emitted helper"))
    }

    pub(crate) fn emitted_callee(
        &self,
        call: solve::CallTarget,
    ) -> Option<Result<perry_hir::types::FuncId>> {
        match call {
            solve::CallTarget::Direct { target, .. } => Some(Ok(self.function(target))),
            solve::CallTarget::Dispatch { helper, .. } => Some(self.helper(helper)),
            solve::CallTarget::ThrowTypeError => None,
        }
    }

    pub(crate) fn storage(
        &self,
        owner: BodyOwner,
        id: perry_hir::types::LocalId,
    ) -> Option<Storage> {
        self.decisions
            .storage(owner, self.local_ids.get(id as usize).copied().flatten()?)
    }

    pub(crate) fn parameter_function(
        &self,
        owner: BodyOwner,
        id: perry_hir::types::LocalId,
    ) -> Option<perry_hir::types::FuncId> {
        match self.storage(owner, id)? {
            Storage::ErasedFunction(target) => target,
            _ => None,
        }
    }

    pub(crate) fn binding_has_any(
        &self,
        id: perry_hir::types::LocalId,
        flag: types::LocalFlags,
    ) -> bool {
        self.local_ids
            .get(id as usize)
            .copied()
            .flatten()
            .is_some_and(|idx| self.local_flags[idx].intersects(flag))
    }

    pub(crate) fn site(&self, owner: BodyOwner, expr: &perry_hir::ir::Expr) -> solve::SiteEntry {
        self.decisions.site(owner, expr).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::bitset::BitSet;
    use super::types::{FuncIdentity, TemplateId, Ty, TypeTable};
    use crate::Absence;
    use perry_hir::types::Type as HirType;
    use pliron::context::Context;
    use std::collections::BTreeMap;

    #[test]
    fn bitset_insert_remove_and_union_across_words() {
        let mut a = BitSet::default();
        assert!(!a.contains(0));
        assert!(a.insert(0));
        assert!(!a.insert(0));
        assert!(a.insert(63));
        assert!(a.insert(64));
        assert!(a.insert(129));
        assert!(a.contains(64));
        assert!(!a.contains(65));

        assert!(a.remove(63));
        assert!(!a.remove(63));
        let mut b = BitSet::default();
        b.insert(5);
        b.insert(129);
        assert!(a.union_with(&b));
        assert!(!a.union_with(&b));
        for bit in 0..=130 {
            assert_eq!(a.contains(bit), [0, 5, 64, 129].contains(&bit));
        }
    }

    #[test]
    fn type_table_canonical_unions_and_joins() {
        let ctx = Context::new();
        let mut table = TypeTable::new();

        let u1 = table.union(&[Ty::STRING, Ty::NUMBER]);
        let u2 = table.union(&[Ty::NUMBER, Ty::STRING, Ty::NUMBER]);
        assert_eq!(u1, u2);

        let u3 = table.union(&[u1, Ty::VOID]);
        let u4 = table.join(Ty::VOID, u2);
        assert_eq!(u3, u4);

        assert_eq!(table.join(Ty::BOTTOM, Ty::NUMBER), Ty::NUMBER);
        assert_eq!(table.join(Ty::NUMBER, Ty::NEVER), Ty::NUMBER);

        let (defined, abs) = table.strip_nullish(u3);
        assert_eq!(defined, u1);
        assert_eq!(abs, Some(Absence::Undefined));

        let without_undef = table.exclude_absences(u3, Absence::Undefined);
        assert_eq!(without_undef, u1);

        let f1 = table.func(FuncIdentity {
            template: TemplateId::test_id(1),
            capture_owner: super::types::CaptureOwner::None,
        });
        let f2 = table.func(FuncIdentity {
            template: TemplateId::test_id(2),
            capture_owner: super::types::CaptureOwner::None,
        });
        assert_ne!(f1, f2);

        let a = table.property("a", Ty::NUMBER, false);
        let b = table.property("b", Ty::STRING, false);
        let o1 = table.object(&[b, a], None);
        let o2 = table.object(&[a, b], None);
        assert_ne!(o1, o2);

        let hir_u = HirType::Union(vec![HirType::String, HirType::Number, HirType::Void]);
        let interned = table.intern_hir(&ctx, &hir_u, true);
        assert_eq!(interned, u3);

        // Verify name interning roundtrips cleanly across many distinct identifiers.
        for i in 0..200u32 {
            let name = format!("prop_{i}");
            let id = table.intern_name(&name);
            assert_eq!(table.intern_name(&name), id);
        }
        for i in 0..200u32 {
            let name = format!("prop_{i}");
            let id = table.intern_name(&name);
            assert_eq!(table.name_str(id), name);
        }
    }

    #[test]
    fn engine_resolves_core_generics_storage_and_classes() {
        let empty_sources = BTreeMap::new();
        let empty_foreign = Default::default();
        let samples = [
            (
                "add.ts",
                "function add(a: number, b: number): number { return a + b; } export function run(): number { return add(1, 2); }",
            ),
            (
                "generic.ts",
                "function id<T>(x: T): T { return x; } export function run(): number { return id(42); }",
            ),
            (
                "class.ts",
                "class Box { constructor(value) { this.value = value; } get() { return this.value; } } console.log(new Box(2).get(), new Box('a').get());",
            ),
            (
                "closure.ts",
                "function make() { let v; const set = (s) => { v = s; }; set('hi'); return v.length; } console.log(make());",
            ),
            (
                "destructured_param.js",
                "function area({ w, h }) { return w * h; } function second([, b]) { return b; } console.log(area({ w: 2, h: 3 }), second([1, 2]));",
            ),
        ];
        for (name, src) in samples {
            let mut ctx = Context::new();
            let module = crate::lower_sources(&mut ctx, src, name, &empty_sources, &empty_foreign)
                .unwrap_or_else(|e| panic!("{name} failed: {e}"));
            assert!(
                !perry_mir::functions(pliron::op::Op::get_operation(&module), &ctx).is_empty(),
                "{name}: expected emitted functions or classes"
            );
        }
    }
}
