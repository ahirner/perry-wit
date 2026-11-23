//! Data-oriented monomorphization and type-inference engine over dense indices.
pub(crate) mod ast_scan;
pub(crate) mod bitset;
pub(crate) mod boundary;
pub(crate) mod builtins;
pub(crate) mod emit;
pub(crate) mod solve;
pub(crate) mod types;

use crate::{hir, hir_compat};
use boundary::{ModuleIndex, SourceFacts};
use perry_hir::ir::Module as HirModule;
use perry_wit_diagnostics::Result;
use pliron::context::Context;
use std::collections::HashMap;
use types::TypeTable;

/// Execute the new inference engine on a lowered HIR module.
///
/// Phase split: borrows `hir` read-only across boundary indexing, worklist solving, and
/// monomorphic emission building, then applies the emitted outputs to `hir` and `metadata`.
pub(crate) fn run_engine(
    ctx: &mut Context,
    hir: &mut HirModule,
    metadata: &mut hir_compat::Metadata<'_>,
    facts: &SourceFacts,
    foreign: &HashMap<String, hir::Foreign>,
) -> Result<()> {
    let mut types = TypeTable::new();
    let emitted = {
        let index = ModuleIndex::build(ctx, hir, facts, &metadata.locations, &mut types);
        let solved = solve::solve(ctx, hir, &index, metadata, facts, foreign, &mut types)?;
        emit::build(ctx, hir, &index, &metadata.locations, &types, &solved)?
    };
    emitted.apply(hir, metadata);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bitset::BitSet;
    use super::types::{FuncIdentity, ObjectProp, TemplateId, Ty, TypeTable};
    use crate::Absence;
    use perry_hir::types::Type as HirType;
    use pliron::context::Context;
    use std::collections::{BTreeMap, HashMap};

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
            template: TemplateId(1),
            capture_owner: None,
        });
        let f2 = table.func(FuncIdentity {
            template: TemplateId(2),
            capture_owner: None,
        });
        assert_ne!(f1, f2);

        let n_a = table.intern_name("a");
        let n_b = table.intern_name("b");
        let o1 = table.object(
            &[
                ObjectProp {
                    name: n_b,
                    ty: Ty::STRING,
                    optional: false,
                    readonly: false,
                },
                ObjectProp {
                    name: n_a,
                    ty: Ty::NUMBER,
                    optional: false,
                    readonly: false,
                },
            ],
            None,
        );
        let o2 = table.object(
            &[
                ObjectProp {
                    name: n_a,
                    ty: Ty::NUMBER,
                    optional: false,
                    readonly: false,
                },
                ObjectProp {
                    name: n_b,
                    ty: Ty::STRING,
                    optional: false,
                    readonly: false,
                },
            ],
            None,
        );
        assert_ne!(o1, o2);

        let hir_u = HirType::Union(vec![HirType::String, HirType::Number, HirType::Void]);
        let interned = table.intern_hir(&ctx, &hir_u, true);
        assert_eq!(interned, u3);
        let roundtrip = table.materialize_hir(&ctx, interned, &|_| None);
        assert!(matches!(roundtrip, HirType::Union(ref members) if members.len() == 3));

        // Verify FlatHashIndex survives multiple rehashes without cycles or lost entries.
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
        let empty_foreign = HashMap::new();
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
            let program = crate::load_sources(&mut ctx, src, name, &empty_sources, &empty_foreign)
                .unwrap_or_else(|e| panic!("{name} failed: {e}"));
            assert!(
                !program.module().functions.is_empty() || !program.module().classes.is_empty(),
                "{name}: expected emitted functions or classes"
            );
        }
    }
}
