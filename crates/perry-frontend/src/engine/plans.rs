//! Sparse expression and binding decisions in each emitted body.
use super::{
    solve::{SiteEntry, SiteResolution, Solved},
    types::{InstanceId, LocalIdx, LocalPlanSpan, PlanSpan, Pool},
};
use crate::source::{BuiltinConstructor, ConstructorSites};
use perry_hir::{ir::Expr, types::FuncId};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct ExprIdx(usize);

#[derive(Default)]
pub(super) struct Expressions {
    sites: HashMap<*const Expr, (ExprIdx, Option<BuiltinConstructor>)>,
    pub(super) declared: HashMap<ExprIdx, SiteResolution>,
}

impl Expressions {
    pub(super) fn end(&self) -> ExprIdx {
        ExprIdx(self.sites.len())
    }

    pub(super) fn register(&mut self, expr: &Expr, sites: &impl ConstructorSites) -> ExprIdx {
        let next = self.end();
        self.sites
            .entry(std::ptr::from_ref(expr))
            .or_insert_with(|| (next, sites.constructor(expr)))
            .0
    }

    pub(super) fn declared(&self, expr: &Expr) -> Option<SiteResolution> {
        self.declared.get(&self.index(expr)?).copied()
    }

    pub(super) fn index(&self, expr: &Expr) -> Option<ExprIdx> {
        self.sites.get(&std::ptr::from_ref(expr)).map(|row| row.0)
    }
}

impl ConstructorSites for Expressions {
    fn constructor(&self, expr: &Expr) -> Option<BuiltinConstructor> {
        self.sites.get(&std::ptr::from_ref(expr))?.1
    }
}

impl ConstructorSites for super::InstancePlans {
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
pub(super) struct Decisions {
    pub(super) expressions: Expressions,
    bodies: HashMap<BodyOwner, Body>,
    sites: Pool<PlanSpan, (ExprIdx, SiteEntry)>,
    locals: Pool<LocalPlanSpan, (LocalIdx, super::Storage)>,
}

fn lookup<K: Ord, V>(rows: &[(K, V)], key: K) -> Option<&V> {
    let pos = rows.binary_search_by(|r| r.0.cmp(&key)).ok()?;
    rows.get(pos).map(|r| &r.1)
}

impl Decisions {
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
        let mut entry = self
            .bodies
            .get(&owner)
            .and_then(|b| lookup(&self.sites[b.sites], index))
            .copied()
            .unwrap_or_default();
        entry.resolution = entry
            .resolution
            .or_else(|| self.expressions.declared.get(&index).copied());
        entry
    }
    pub(super) fn storage(&self, owner: BodyOwner, id: LocalIdx) -> Option<super::Storage> {
        lookup(&self.locals[self.bodies.get(&owner)?.locals], id).copied()
    }
}
