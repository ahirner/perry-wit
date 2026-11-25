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
    Function {
        identity: types::FuncIdentity,
        environment: Option<perry_hir::types::LocalId>,
    },
    Widen {
        local: perry_hir::types::LocalId,
        from: types::Ty,
        to: types::Ty,
    },
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
    pub(crate) strings: crate::source::StringConstants<'s>,
    pub(crate) bodies: crate::source::InstanceBodies<'s>,
    pub(crate) sparse_arrays: bool,
    pub(crate) locations: types::IdxVec<types::LocId, types::Ty>,
    pub(crate) dict_reads: bitset::BitSet,
    pub(crate) capture_free: bitset::BitSet,
    pub(crate) escaping_env_templates: bitset::BitSet,
    pub(crate) instances: InstancePlans,
}

enum InstantiationOrigin {
    Instance(types::InstanceId),
    Dispatch {
        function: perry_hir::types::FuncId,
        offset: u32,
    },
}

pub(crate) fn lower_module(
    ctx: &mut Context,
    mut source: SourceModule<'_>,
    world: &crate::wit::WorldTypes<'_>,
) -> Result<crate::LoweredProgram> {
    let mut types = TypeTable::new();
    let mut index = ModuleIndex::build(
        ctx,
        &source.bodies,
        &source.declarations,
        &source.locations,
        &source.closure_display_names,
        &source.constructors,
        &mut types,
    );
    let mut solved = solve::solve(ctx, &source, &index, world, &mut types)?;
    let mut origins = Vec::new();
    let mut records = perry_mir::RecordLayouts::default();
    let (bodies, mut instances, decisions) = instances::build(
        &source.bodies,
        &source.declarations,
        &source.locations,
        &index,
        &types,
        &solved,
        &mut origins,
        (ctx, &mut records),
    )?;
    instances.decisions = plans::Decisions::from_solver(&mut index, &mut solved, decisions);
    for origin in origins {
        match origin {
            InstantiationOrigin::Instance(id) => {
                let inst = &solved.instances[id];
                let caller = inst
                    .caller
                    .map(|id| &solved.instances[id])
                    .filter(|caller| caller.template != types::TemplateId::INIT);
                source.locations.record_instantiation(
                    inst.emitted_name.clone(),
                    index.view(inst.template).raw_name(),
                    inst.call_byte_offset,
                    caller.map(|c| c.emitted_name.clone()),
                );
            }
            InstantiationOrigin::Dispatch { function, offset } => {
                source.locations.record_instantiation(
                    format!("__dispatch_{function}"),
                    "dispatch".into(),
                    offset,
                    None,
                );
            }
        }
    }
    instances.local_ids = index.local_to_idx;
    instances.local_flags = index.locals.map(|local| local.flags);
    let strings = index.strings;
    let capture_free = index.capture_free_templates;
    for (witness, instance) in source.witnesses.iter_mut().zip(&solved.witness_instances) {
        witness.target = instance.map(|id| solved.instances[id].emitted_func_id);
    }
    let specialization = Specialization {
        iteration: index.iteration,
        literal_classes: index.literal_classes,
        annotations: index.annotations,
        lexical: index.functions,
        strings,
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
        world,
        &mut source.locations,
        &source.witnesses,
        &specialization,
        (types, records),
    )
}

impl Specialization<'_> {
    pub(crate) fn regexp_operand(&self, expr: &perry_hir::ir::Expr) -> Option<String> {
        self.strings.regexp_operand(expr, &self.instances.local_ids)
    }
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

    pub(crate) fn module_binding(&self, id: perry_hir::types::LocalId) -> bool {
        self.local_ids
            .get(id as usize)
            .copied()
            .flatten()
            .is_some_and(|idx| self.local_flags[idx].contains(types::LocalFlags::MODULE))
    }

    pub(crate) fn site(&self, owner: BodyOwner, expr: &perry_hir::ir::Expr) -> solve::SiteEntry {
        self.decisions.site(owner, expr).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::bitset::BitSet;
    use super::types::{FuncIdentity, ObjectProp, TemplateId, Ty, TypeTable};
    use crate::Absence;
    use perry_hir::types::Type as HirType;
    use pliron::context::Context;
    use std::collections::BTreeMap;

    #[test]
    fn bitset_operations_and_iteration() {
        let mut a = BitSet::default();
        assert!(a.is_empty());
        assert!(a.insert(0));
        assert!(!a.insert(0));
        assert!(a.insert(63));
        assert!(a.insert(64));
        assert!(a.insert(129));
        assert_eq!(a.iter().count(), 4);
        assert!(a.contains(64));
        assert!(!a.contains(65));
        let bits: Vec<usize> = a.iter().collect();
        assert_eq!(bits, vec![0, 63, 64, 129]);

        assert!(a.remove(63));
        assert!(!a.remove(63));
        let mut b = BitSet::default();
        b.insert(5);
        b.insert(129);
        assert!(a.union_with(&b));
        assert!(!a.union_with(&b));
        assert_eq!(a.iter().collect::<Vec<_>>(), vec![0, 5, 64, 129]);
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

        let (n_a, n_b) = (table.intern_name("a"), table.intern_name("b"));
        let prop = |name, ty| ObjectProp {
            name,
            ty,
            optional: false,
            readonly: false,
        };
        let o1 = table.object(&[prop(n_b, Ty::STRING), prop(n_a, Ty::NUMBER)], None);
        let o2 = table.object(&[prop(n_a, Ty::NUMBER), prop(n_b, Ty::STRING)], None);
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
