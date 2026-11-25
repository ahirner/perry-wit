//! Data-oriented monomorphization and type-inference engine over dense indices.
pub(crate) mod bitset;
pub(crate) mod boundary;
pub(crate) mod builtins;
pub(crate) mod emit;
pub(crate) mod solve;
pub(crate) mod types;

use crate::{hir_compat, wit};
use boundary::ModuleIndex;
use perry_hir::ir::Module as HirModule;
use perry_wit_diagnostics::Result;
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
    Value(types::Ty),
    Boxed { ty: types::Ty, init: CellInit },
    Promise(types::Ty),
    AbortController,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ExpressionOp {
    Function {
        identity: types::FuncIdentity,
        target: types::Ty,
    },
    Matches {
        identity: types::FuncIdentity,
        source: types::Ty,
        equal: bool,
    },
    Expected(types::Ty),
    Map(types::Ty),
    Set(types::Ty),
    Promise {
        settled: types::Ty,
        executor: NativeCallback,
        handles: [Option<types::FuncIdentity>; 2],
    },
    StartTimer(perry_hir::types::FuncId),
    ClearTimer,
}

pub(crate) struct RecordLayout {
    pub(crate) function: Option<types::FuncIdentity>,
    pub(crate) name: String,
    pub(crate) fields: Vec<(String, Storage)>,
}

pub(crate) struct CallbackBinding {
    pub(crate) call: NativeCallback,
    pub(crate) params: Vec<Option<types::Ty>>,
}

pub(crate) enum NativeOperation {
    Callback(CallbackBinding),
    Callable(solve::CallableOp),
    Allocate(RecordLayout),
    Expression(ExpressionOp),
    Resource(usize),
}

pub(crate) struct InstanceSignature {
    pub(crate) params: Vec<Option<types::Ty>>,
    pub(crate) result: Option<types::Ty>,
}

type InstanceSignatures = HashMap<perry_hir::types::FuncId, InstanceSignature>;
type NativeOperations = HashMap<perry_hir::types::FuncId, NativeOperation>;

pub(crate) struct DispatchArm {
    pub(crate) identity: types::FuncIdentity,
    pub(crate) target: perry_hir::types::FuncId,
    pub(crate) receiver: bool,
    pub(crate) arguments: usize,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CallbackOperand {
    None,
    Environment,
    Callee,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NativeCallback {
    ThrowTypeError,
    Call {
        target: perry_hir::types::FuncId,
        operand: CallbackOperand,
        receiver: bool,
    },
}

impl NativeCallback {
    pub(crate) fn takes_value(self) -> bool {
        matches!(
            self,
            Self::Call {
                operand: CallbackOperand::Callee | CallbackOperand::Environment,
                ..
            }
        )
    }
}

pub(crate) enum Reaction {
    Chain {
        fulfilled: Option<NativeCallback>,
        rejected: Option<NativeCallback>,
        rejects_only: bool,
    },
    Finally(NativeCallback),
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
        callback: NativeCallback,
    },
}

pub(crate) struct NativeFunction {
    pub(crate) name: String,
    pub(crate) params: Vec<NativeParam>,
    pub(crate) result: types::Ty,
    pub(crate) is_async: bool,
    pub(crate) body: NativeBody,
}

#[derive(Default)]
pub(crate) struct ClassFields {
    pub(crate) instance: HashMap<String, types::Ty>,
    pub(crate) statics: HashMap<String, types::Ty>,
}

pub(crate) struct Capture {
    pub(crate) local: perry_hir::types::LocalId,
    pub(crate) field: String,
    pub(crate) storage: Storage,
}

#[derive(Default)]
pub(crate) struct InstancePlans {
    pub(crate) per_iteration: std::collections::HashSet<perry_hir::types::LocalId>,
    pub(crate) boxed_parameters:
        HashMap<(perry_hir::types::FuncId, perry_hir::types::LocalId), types::Ty>,
    pub(crate) captures: HashMap<perry_hir::types::LocalId, Vec<Capture>>,
    pub(crate) operations: NativeOperations,
    pub(crate) class_fields: HashMap<String, ClassFields>,
    pub(crate) locals:
        HashMap<(Option<perry_hir::types::FuncId>, perry_hir::types::LocalId), Storage>,
    pub(crate) signatures: InstanceSignatures,
    pub(crate) closures: InstanceSignatures,
    pub(crate) functions: std::collections::BTreeMap<perry_hir::types::FuncId, NativeFunction>,
}

pub(crate) struct Specialization {
    pub(crate) sparse_arrays: bool,
    pub(crate) types: std::cell::RefCell<TypeTable>,
    pub(crate) locations: types::IdxVec<types::LocId, types::Ty>,
    pub(crate) dict_reads: bitset::BitSet,
    pub(crate) capture_free: bitset::BitSet,
    pub(crate) class_names: types::IdxVec<types::ClassInstId, String>,
    pub(crate) escaping_envs: bitset::BitSet,
    pub(crate) instances: InstancePlans,
}

pub(crate) fn run_engine(
    ctx: &mut Context,
    hir: &mut HirModule,
    metadata: &mut hir_compat::Metadata<'_>,
    foreign: &HashMap<String, wit::Foreign>,
) -> Result<Specialization> {
    let mut types = TypeTable::new();
    let index = ModuleIndex::build(ctx, hir, &metadata.source, &metadata.locations, &mut types);
    let solved = solve::solve(ctx, hir, &index, metadata, foreign, &mut types)?;
    for (witness, instance) in metadata.witnesses.iter_mut().zip(&solved.witness_instances) {
        witness.target = instance.map(|id| solved.instances[id].emitted_func_id);
    }
    let emitted = emit::build(ctx, hir, &index, metadata, &types, &solved)?;
    let capture_free = index.capture_free_templates;
    let instances = emitted.apply(hir, metadata);
    Ok(Specialization {
        sparse_arrays: metadata.source.sparse_arrays || solved.deletes_array_elements,
        types: types.into(),
        escaping_envs: solved.escaping_env_templates,
        locations: solved.locations,
        dict_reads: solved.dict_reads,
        class_names: solved
            .class_insts
            .map(|instance| instance.emitted_name.clone()),
        capture_free,
        instances,
    })
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
