//! Data-oriented monomorphization and type-inference engine over dense indices.
pub(crate) type BitSet = bit_set::BitSet<u64>;
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

#[derive(Clone, Copy)]
pub(crate) enum NativeParam {
    Value(types::Ty),
    Argument { position: usize, ty: types::Ty },
    Environment(types::FuncIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TimerSchedule {
    pub(crate) repeats: bool,
    pub(crate) has_delay: bool,
}

pub(crate) enum NativeBody {
    Resource(crate::hir_compat::Target<types::InstanceId>),
    Settle(perry_mir::SettleKind),
    Helper(solve::HelperSpec),
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
    Receiver(types::Ty),
    Settler(types::Ty),
    Unit,
}

impl EnvironmentField {
    pub(crate) fn storage(&self) -> Storage {
        match *self {
            Self::Capture { ty, cell, .. } => {
                cell.map_or(Storage::Value(ty), |init| Storage::Boxed { ty, init })
            }
            Self::Receiver(ty) => Storage::Value(ty),
            Self::Settler(ty) => Storage::Promise(ty),
            Self::Unit => Storage::Value(types::Ty::NUMBER),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Environment {
    pub(crate) record: pliron::r#type::TypedHandle<perry_mir::RecordType>,
    pub(crate) fields: types::EnvFieldSpan,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClosureValue {
    pub(crate) identity: types::FuncIdentity,
    pub(crate) conversions: types::CaptureConversionSpan,
}

#[derive(Default)]
pub(crate) struct CallTargets<T> {
    instances: types::IdxVec<types::InstanceId, Option<T>>,
    helpers: types::IdxVec<types::HelperId, Option<T>>,
}

impl<T: Copy> CallTargets<T> {
    pub(crate) fn bind<U>(&self, f: impl Fn(T) -> Option<U>) -> CallTargets<U> {
        CallTargets {
            instances: self.instances.map(|id| id.and_then(&f)),
            helpers: self.helpers.map(|id| id.and_then(&f)),
        }
    }

    pub(crate) fn instance(&self, id: types::InstanceId) -> Result<T> {
        self.instances[id]
            .ok_or_else(|| perry_wit_diagnostics::internal!("missing emitted instance {id:?}"))
    }

    pub(crate) fn helper(&self, id: types::HelperId) -> Result<T> {
        self.helpers[id]
            .ok_or_else(|| perry_wit_diagnostics::internal!("missing emitted helper {id:?}"))
    }

    pub(crate) fn callee(&self, target: solve::FunctionTarget) -> Result<T> {
        use solve::FunctionTarget::*;
        match target {
            Direct(id) | Closure(id) => self.instance(id),
            Dispatcher(id) => self.helper(id),
        }
    }
}

#[derive(Default)]
pub(crate) struct InstancePlans<'s> {
    pub(crate) dispatch_arms: types::Pool<types::DispatchArmSpan, solve::DispatchArm>,
    decisions: plans::Decisions<'s>,
    pub(crate) targets: CallTargets<perry_hir::types::FuncId>,
    pub(crate) locals: crate::hir_compat::Locals<'s>,
    pub(crate) capture_conversions: types::Pool<
        types::CaptureConversionSpan,
        (perry_hir::types::LocalId, solve::ValueCoercion),
    >,
    pub(crate) environment_fields: types::Pool<types::EnvFieldSpan, EnvironmentField>,
    pub(crate) environments: indexmap::IndexMap<types::FuncIdentity, Environment>,
}

pub(crate) struct Specialization<'s> {
    pub(crate) templates: types::IdxVec<types::TemplateId, boundary::TemplateView<'s>>,
    function_templates: Vec<Option<types::TemplateId>>,
    pub(crate) writes: crate::narrowing::WriteEffects,
    pub(crate) bodies: crate::source::InstanceBodies<'s>,
    pub(crate) sparse_arrays: bool,
    pub(crate) locations: types::IdxVec<types::LocId, types::Ty>,
    pub(crate) dict_reads: BitSet,
    pub(crate) capture_free: BitSet,

    pub(crate) instances: InstancePlans<'s>,
    pub(crate) resources: crate::hir_compat::ResourceClasses<types::ClassInstId>,
}

impl<'s> Specialization<'s> {
    pub(crate) fn closure(
        &self,
        id: perry_hir::types::FuncId,
    ) -> Option<(&boundary::TemplateView<'s>, plans::ExprIdx)> {
        let template = self
            .function_templates
            .get(id as usize)
            .copied()
            .flatten()?;
        let view = &self.templates[template];
        let boundary::TemplateKind::Closure(site) = view.kind else {
            return None;
        };
        Some((view, site))
    }
    pub(crate) fn closure_value(
        &self,
        owner: BodyOwner,
        id: perry_hir::types::FuncId,
    ) -> Option<ClosureValue> {
        let (_, index) = self.closure(id)?;
        self.instances.decisions.site_at(owner, index).environment()
    }
}

pub(crate) fn lower_module<'r>(
    ctx: &mut Context,
    mut source: SourceModule<'_>,
    exports: Vec<crate::hir_compat::SourceWitness<'r>>,
    world: &crate::wit::WorldTypes<'_>,
) -> Result<crate::LoweredProgram<'r>> {
    let mut types = TypeTable::new();
    let decls = crate::shapes::Declarations::new(
        ctx,
        &source.declarations.interfaces,
        &source.declarations.type_aliases,
        &source.declarations.imports,
        &source.bodies.classes,
        world,
        &source.locations,
    )?;
    let mut index = ModuleIndex::build(
        ctx,
        &source.bodies,
        &source.declarations,
        &source.locations,
        &decls,
        &mut types,
    );
    let resources = std::mem::take(&mut source.resources);
    let (solved, exports) = solve::solve(
        ctx,
        &source,
        (exports, resources),
        &index,
        &decls,
        &mut types,
    )?;
    let mut records = perry_mir::RecordLayouts::default();
    let (bodies, mut instances, exports) = instances::build(
        &source.bodies,
        &mut source.locations,
        &mut index,
        &types,
        (&solved, exports),
        (ctx, &mut records),
    )?;
    drop(solved.inst_sites);
    instances.dispatch_arms = solved.dispatch_arms;
    let capture_free = index.capture_free_templates;
    let specialization = Specialization {
        resources: solved.resources,
        templates: index.views,
        function_templates: index.functions,
        writes: solved.writes,
        bodies,
        sparse_arrays: source.sparse_arrays || solved.deletes_array_elements,
        locations: solved.locations,
        dict_reads: solved.dict_reads,
        capture_free,

        instances,
    };
    crate::lower::program::build_module(
        ctx,
        &source.declarations,
        decls,
        &mut source.locations,
        &specialization,
        exports,
        (types, records),
    )
}

impl<'s> InstancePlans<'s> {
    pub(crate) fn expressions(&self) -> &plans::Expressions<'s> {
        &self.decisions.expressions
    }

    pub(crate) fn storage(
        &self,
        owner: BodyOwner,
        id: perry_hir::types::LocalId,
    ) -> Option<Storage> {
        self.decisions.storage(owner, self.locals.index(id)?)
    }

    pub(crate) fn globals(&self) -> impl Iterator<Item = perry_hir::types::LocalId> + '_ {
        self.locals
            .rows
            .iter()
            .filter(|local| local.flags.contains(types::LocalFlags::GLOBAL_SLOT))
            .map(|local| local.id)
    }
    pub(crate) fn local(&self, id: perry_hir::types::LocalId) -> Result<&types::LocalInfo<'s>> {
        Ok(&self.locals.rows[self.local_id(id)?])
    }
    fn local_id(&self, id: perry_hir::types::LocalId) -> Result<types::LocalIdx> {
        self.locals
            .index(id)
            .ok_or_else(|| perry_wit_diagnostics::internal!("unindexed local {id}"))
    }

    pub(crate) fn site(
        &self,
        owner: BodyOwner,
        expr: &perry_hir::ir::Expr,
    ) -> Result<solve::SiteEntry> {
        self.decisions
            .site(owner, expr)
            .ok_or_else(|| perry_wit_diagnostics::internal!("unindexed HIR expression {expr:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::types::{FuncIdentity, TemplateId, Ty, TypeTable};
    use crate::Absence;
    use perry_hir::types::Type as HirType;
    use pliron::context::Context;
    use std::collections::BTreeMap;

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
