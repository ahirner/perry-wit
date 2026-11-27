//! Sparse expression and binding decisions in each emitted body.
use super::{
    solve::{SiteEntry, SiteResolution, Solved},
    types::{DeclaredId, IdxVec, InstanceId, LocalIdx, LocalPlanSpan, PlanSpan, Pool},
};
use crate::source::{BuiltinConstructor, ConstructorSites};
use perry_hir::{ir::Expr, types::FuncId};
use std::collections::HashMap;

pub(crate) use super::types::ExprIdx;

pub(super) struct SiteRows<V>(pub(super) Vec<(ExprIdx, V)>);

impl<V> Default for SiteRows<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> SiteRows<V> {
    pub(super) fn get(&self, site: ExprIdx) -> Option<&V> {
        lookup(&self.0, site)
    }

    pub(super) fn get_mut(&mut self, site: ExprIdx) -> Option<&mut V> {
        let pos = self.0.binary_search_by_key(&site, |r| r.0).ok()?;
        Some(&mut self.0.get_mut(pos)?.1)
    }

    pub(super) fn entry(&mut self, site: ExprIdx, initial: impl FnOnce() -> V) -> &mut V {
        let pos = self
            .0
            .binary_search_by_key(&site, |r| r.0)
            .unwrap_or_else(|pos| {
                self.0.insert(pos, (site, initial()));
                pos
            });
        &mut self.0[pos].1
    }
    pub(super) fn set(&mut self, site: ExprIdx, value: V)
    where
        V: Copy,
    {
        *self.entry(site, || value) = value;
    }
    pub(super) fn range(&self, sites: std::ops::Range<ExprIdx>) -> &[(ExprIdx, V)] {
        let start = self.0.partition_point(|r| r.0 < sites.start);
        let end = self.0.partition_point(|r| r.0 < sites.end);
        &self.0[start..end]
    }
}

#[derive(Clone, Copy)]
enum SourcePlan {
    LowerHir,
    Resolved(DeclaredId),
}

#[derive(Default)]
pub(crate) struct Expressions<'h> {
    sites: HashMap<*const Expr, ExprIdx>,
    declared: IdxVec<DeclaredId, SiteResolution>,
    source: IdxVec<ExprIdx, SourcePlan>,
    pub(super) symbols: SiteRows<crate::shapes::TypeName>,
    pub(super) static_fields: SiteRows<LocalIdx>,
    pub(super) literals: SiteRows<&'h [perry_hir::ir::ClassField]>,
    constructors: SiteRows<BuiltinConstructor>,
    pub(super) tuple_positions: SiteRows<usize>,
    pub(super) annotations: SiteRows<super::types::Ty>,
    pub(super) receivers: SiteRows<crate::wit::MemberGroupId>,
}

impl<'h> Expressions<'h> {
    pub(crate) fn tuple_position(&self, index: &Expr) -> Option<usize> {
        crate::hir_compat::tuple_position(index)
            .or_else(|| self.tuple_positions.get(self.index(index)?).copied())
    }

    pub(crate) fn annotation(
        &self,
        expr: &Expr,
    ) -> perry_wit_diagnostics::Result<super::types::Ty> {
        self.index(expr)
            .and_then(|id| self.annotations.get(id).copied())
            .ok_or_else(|| perry_wit_diagnostics::internal!("missing expression type annotation"))
    }
    pub(crate) fn symbol<'a>(
        &self,
        decls: &'a crate::shapes::Declarations<'h>,
        expr: &Expr,
    ) -> Option<&'a crate::shapes::Symbol<'h>> {
        decls.symbol(*self.symbols.get(self.index(expr)?)?)
    }
    pub(crate) fn static_field(&self, expr: &Expr) -> Option<LocalIdx> {
        self.static_fields.get(self.index(expr)?).copied()
    }
    pub(crate) fn literal(&self, expr: &Expr) -> Option<&'h [perry_hir::ir::ClassField]> {
        self.literals.get(self.index(expr)?).copied()
    }

    pub(super) fn end(&self) -> ExprIdx {
        self.source.next_id()
    }

    pub(super) fn register(&mut self, expr: &Expr) -> ExprIdx {
        *self
            .sites
            .entry(std::ptr::from_ref(expr))
            .or_insert_with(|| {
                let site = self.source.push(SourcePlan::LowerHir);
                if let Some(constructor) = crate::source::Constructors.constructor(expr) {
                    self.constructors.set(site, constructor);
                }
                site
            })
    }

    pub(super) fn declare(&mut self, site: ExprIdx, value: SiteResolution) {
        match self.source[site] {
            SourcePlan::Resolved(id) => self.declared[id] = value,
            SourcePlan::LowerHir => {
                self.source[site] = SourcePlan::Resolved(self.declared.push(value))
            }
        }
    }
    fn source_resolution(&self, site: ExprIdx) -> Option<SiteResolution> {
        match self.source[site] {
            SourcePlan::Resolved(id) => Some(self.declared[id]),
            SourcePlan::LowerHir => None,
        }
    }
    pub(super) fn declared(&self, expr: &Expr) -> Option<SiteResolution> {
        self.source_resolution(self.index(expr)?)
    }

    pub(crate) fn receiver(&self, expr: &Expr) -> Option<crate::wit::MemberGroupId> {
        self.receivers.get(self.index(expr)?).copied()
    }

    pub(super) fn index(&self, expr: &Expr) -> Option<ExprIdx> {
        self.sites.get(&std::ptr::from_ref(expr)).copied()
    }
}

impl ConstructorSites for Expressions<'_> {
    fn constructor(&self, expr: &Expr) -> Option<BuiltinConstructor> {
        self.constructors.get(self.index(expr)?).copied()
    }
}

impl ConstructorSites for super::InstancePlans<'_> {
    fn constructor(&self, expr: &Expr) -> Option<BuiltinConstructor> {
        self.decisions.expressions.constructor(expr)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BodyOwner {
    Initializer,
    Function(FuncId),
}

impl Solved {
    pub(super) fn body_owner(&self, instance: InstanceId) -> BodyOwner {
        if instance == self.init_inst {
            BodyOwner::Initializer
        } else {
            BodyOwner::Function(self.instances[instance].emitted_func_id)
        }
    }
}

struct Body {
    sites: PlanSpan,
    locals: LocalPlanSpan,
}

#[derive(Default)]
pub(super) struct Decisions<'h> {
    pub(super) expressions: Expressions<'h>,
    bodies: HashMap<BodyOwner, Body>,
    sites: Pool<PlanSpan, (ExprIdx, SiteEntry)>,
    locals: Pool<LocalPlanSpan, (LocalIdx, super::Storage)>,
}

fn lookup<K: Ord, V>(rows: &[(K, V)], key: K) -> Option<&V> {
    let pos = rows.binary_search_by(|r| r.0.cmp(&key)).ok()?;
    rows.get(pos).map(|r| &r.1)
}

impl Decisions<'_> {
    pub(super) fn record_body(
        &mut self,
        owner: BodyOwner,
        sites: impl IntoIterator<Item = (ExprIdx, SiteEntry)>,
        locals: impl IntoIterator<Item = (LocalIdx, super::Storage)>,
    ) {
        let body = Body {
            sites: self.sites.extend(sites),
            locals: self.locals.extend(locals),
        };
        self.bodies.insert(owner, body);
    }

    pub(super) fn site(&self, owner: BodyOwner, expr: &Expr) -> Option<SiteEntry> {
        Some(self.site_at(owner, self.expressions.index(expr)?))
    }

    pub(super) fn site_at(&self, owner: BodyOwner, index: ExprIdx) -> SiteEntry {
        let source = self.expressions.source_resolution(index);
        match self
            .bodies
            .get(&owner)
            .and_then(|body| lookup(&self.sites[body.sites], index))
        {
            Some(entry) => SiteEntry {
                resolution: entry.resolution.or(source),
                ..*entry
            },
            None => SiteEntry {
                erased: false,
                operand: None,
                resolution: source,
            },
        }
    }
    pub(super) fn storage(&self, owner: BodyOwner, id: LocalIdx) -> Option<super::Storage> {
        lookup(&self.locals[self.bodies.get(&owner)?.locals], id).copied()
    }
}
