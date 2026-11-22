//! Data-oriented monomorphization and type-inference engine over dense indices.
pub(crate) mod bitset;
pub(crate) mod boundary;
pub(crate) mod builtins;
pub(crate) mod emit;
pub(crate) mod solve;
pub(crate) mod types;

#[cfg(test)]
pub(crate) mod oracle;

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
        emit::build(ctx, hir, &index, &types, &solved)?
    };
    emitted.apply(hir, metadata);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bitset::BitSet;
    use super::oracle;
    use super::types::{FuncIdentity, ObjectProp, TemplateId, Ty, TypeTable};
    use crate::Absence;
    use perry_hir::types::Type as HirType;
    use pliron::context::Context;
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;

    #[test]
    fn bitset_operations_and_iteration() {
        let mut a = BitSet::with_bits(130);
        assert!(a.is_empty());
        assert!(a.insert(0));
        assert!(!a.insert(0));
        assert!(a.insert(63));
        assert!(a.insert(64));
        assert!(a.insert(129));
        assert_eq!(a.count_ones(), 4);
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
            &mut [
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
            &mut [
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
        assert_eq!(o1, o2);

        let hir_u = HirType::Union(vec![HirType::String, HirType::Number, HirType::Void]);
        let interned = table.intern_hir(&ctx, &hir_u, true);
        assert_eq!(interned, u3);
        let roundtrip = table.materialize_hir(&ctx, interned, &|_| None);
        assert_eq!(
            oracle::canonical_hir_type(&roundtrip),
            oracle::canonical_hir_type(&hir_u)
        );

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
    fn boundary_and_oracle_run_over_acceptance_and_corpus() {
        let empty_sources = BTreeMap::new();
        let empty_foreign = HashMap::new();

        // Always test core workspace-style snippets so CI verifies the oracle unconditionally.
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
                "class Box { constructor(value) { this.value = value; } get() { return this.value; } } console.log(new Box(2).get());",
            ),
            (
                "closure.ts",
                "function make() { let v; const set = (s) => { v = s; }; set('hi'); return v.length; } console.log(make());",
            ),
        ];
        for (name, src) in samples {
            let report = oracle::compare_source(src, name, &empty_sources, &empty_foreign);
            assert!(report.template_count >= 1, "{name}: expected templates");
        }

        // When the local verification harness is present, run the oracle across all 31 acceptance
        // programs and the 88 jco corpus entries to verify boundary indexing never panics.
        let harness = Path::new("/Users/dafcok/Downloads/perry-wit-harness");
        if !harness.exists() {
            return;
        }
        let dump = std::env::var_os("PERRY_ORACLE_DUMP").is_some();
        let mut checked_probes = 0usize;
        for dir in ["probe/accept", "probe/plan2", "probe/method"] {
            let path = harness.join(dir);
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for file in paths {
                let ext = file.extension().and_then(|e| e.to_str());
                if !matches!(ext, Some("js" | "ts")) {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let label = file.file_name().and_then(|n| n.to_str()).unwrap_or("probe");
                let report = oracle::compare_source(&src, label, &empty_sources, &empty_foreign);
                if dump {
                    eprintln!(
                        "PROBE {dir}/{label}: old_err={:?} new_err={:?} diffs={}",
                        report
                            .old_err
                            .as_deref()
                            .map(|s| s.lines().next().unwrap_or("")),
                        report
                            .new_err
                            .as_deref()
                            .map(|s| s.lines().next().unwrap_or("")),
                        report.diffs.len()
                    );
                    for d in &report.diffs {
                        eprintln!(
                            "  [{}] {}: old={:?} new={:?}",
                            d.category, d.key, d.old, d.new
                        );
                    }
                }
                assert!(
                    report.template_count >= 1 || report.new_err.is_some(),
                    "{label}: boundary must index at least module init or report a diagnostic"
                );
                checked_probes += 1;
            }
        }
        assert_eq!(checked_probes, 31, "expected all 31 acceptance probes");

        let corpus_tsv = harness.join("corpus/corpus.tsv");
        if let Ok(tsv) = std::fs::read_to_string(&corpus_tsv) {
            let mut checked_corpus = 0usize;
            let mut corpus_diffs = 0usize;
            let mut old_only_err = 0usize;
            let mut new_only_err = 0usize;
            for line in tsv.lines() {
                let mut cols = line.split('\t');
                let Some(name) = cols.next() else {
                    continue;
                };
                let Some(mode) = cols.next() else {
                    continue;
                };
                if mode != "run" {
                    continue;
                }
                let Some(src_path) = cols.next() else {
                    continue;
                };
                let Ok(src) = std::fs::read_to_string(src_path) else {
                    continue;
                };
                let report = oracle::compare_source(&src, name, &empty_sources, &empty_foreign);
                if dump {
                    if report.old_err.is_some() && report.new_err.is_none() {
                        old_only_err += 1;
                        eprintln!(
                            "CORPUS {name}: OLD_ERR_ONLY old={:?}",
                            report
                                .old_err
                                .as_deref()
                                .map(|s| s.lines().next().unwrap_or(""))
                        );
                    } else if report.old_err.is_none() && report.new_err.is_some() {
                        new_only_err += 1;
                        eprintln!(
                            "CORPUS {name}: NEW_ERR_ONLY new={:?}",
                            report
                                .new_err
                                .as_deref()
                                .map(|s| s.lines().next().unwrap_or(""))
                        );
                    } else if !report.diffs.is_empty() {
                        corpus_diffs += 1;
                        eprintln!("CORPUS {name}: diffs={}", report.diffs.len());
                        for d in report.diffs.iter().take(6) {
                            eprintln!(
                                "  [{}] {}: old={:?} new={:?}",
                                d.category, d.key, d.old, d.new
                            );
                        }
                    }
                }
                checked_corpus += 1;
            }
            if dump {
                eprintln!(
                    "CORPUS SUMMARY: checked={checked_corpus} old_only_err={old_only_err} new_only_err={new_only_err} with_diffs={corpus_diffs}"
                );
            }
            assert!(
                checked_corpus >= 80,
                "expected jco corpus entries, got {checked_corpus}"
            );
        }
    }
}
