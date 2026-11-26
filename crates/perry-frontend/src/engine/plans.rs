//! Sparse expression and binding decisions in each emitted body.
use super::{
    boundary::ModuleIndex,
    solve::{SiteEntry, SiteResolution, Solved},
    types::{Idx, InstanceId, LocalIdx, LocalPlanSpan, PlanSpan, Pool},
};
use crate::source::{BuiltinConstructor, ConstructorSites};
use perry_hir::{ir::Expr, types::FuncId};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct ExprIdx(usize);

#[derive(Default)]
pub(super) struct Expressions {
    sites: HashMap<*const Expr, (ExprIdx, Option<BuiltinConstructor>)>,
    pub(super) declared: HashMap<ExprIdx, SiteResolution>,
}

impl Expressions {
    pub(super) fn register(&mut self, expr: &Expr, sites: &impl ConstructorSites) -> ExprIdx {
        let next = ExprIdx(self.sites.len());
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

#[derive(Default)]
pub(super) struct PendingBody {
    sites: PlanSpan,
    pub(super) locals: BTreeMap<LocalIdx, super::Storage>,
}

struct Body {
    sites: PlanSpan,
    locals: LocalPlanSpan,
}

pub(super) type Pending = BTreeMap<BodyOwner, PendingBody>;

#[derive(Default)]
pub(super) struct Decisions {
    expressions: Expressions,
    bodies: Vec<(BodyOwner, Body)>,
    sites: Pool<PlanSpan, (ExprIdx, SiteEntry)>,
    locals: Pool<LocalPlanSpan, (LocalIdx, super::Storage)>,
}

fn lookup<K: Ord, V>(rows: &[(K, V)], key: K) -> Option<&V> {
    let pos = rows.binary_search_by(|r| r.0.cmp(&key)).ok()?;
    rows.get(pos).map(|r| &r.1)
}

impl Decisions {
    pub(super) fn from_solver(
        index: &mut ModuleIndex<'_>,
        solved: &mut Solved,
        mut pending: Pending,
    ) -> Self {
        let mut sites = Pool::default();
        for (id, inst) in solved.instances.iter_enumerated() {
            if !solved.live_instances.contains(id.index()) {
                continue;
            }
            let owner = solved.body_owner(id);
            let entries = std::mem::take(&mut solved.inst_sites[id]);
            let resolved =
                index
                    .sites(inst.template)
                    .iter()
                    .zip(entries)
                    .filter_map(|(source, mut entry)| {
                        if matches!(
                            entry.resolution,
                            Some(SiteResolution::Cell(_) | SiteResolution::Closure(_))
                        ) {
                            entry.resolution = None;
                        }
                        (entry.erased || entry.coercion.is_some() || entry.resolution.is_some())
                            .then_some((source.index, entry))
                    });
            pending.entry(owner).or_default().sites = sites.extend(resolved);
        }
        let mut locals = Pool::default();
        let bodies = pending
            .into_iter()
            .map(|(owner, row)| {
                (
                    owner,
                    Body {
                        sites: row.sites,
                        locals: locals.extend(row.locals),
                    },
                )
            })
            .collect();
        Self {
            expressions: std::mem::take(&mut index.expressions),
            bodies,
            sites,
            locals,
        }
    }

    pub(super) fn site(&self, owner: BodyOwner, expr: &Expr) -> Option<SiteEntry> {
        let index = self.expressions.index(expr)?;
        let mut entry = lookup(&self.bodies, owner)
            .and_then(|b| lookup(&self.sites[b.sites], index))
            .copied()
            .unwrap_or_default();
        entry.resolution = entry
            .resolution
            .or_else(|| self.expressions.declared.get(&index).copied());
        Some(entry)
    }
    pub(super) fn storage(&self, owner: BodyOwner, id: LocalIdx) -> Option<super::Storage> {
        lookup(&self.locals[lookup(&self.bodies, owner)?.locals], id).copied()
    }
}

use super::{
    EnvironmentField,
    types::{EnvFieldSpan, EnvId, FuncIdentity, IdxVec},
};
use perry_mir::RecordType;
use pliron::r#type::TypedHandle;

#[derive(Default)]
pub(crate) struct Environments {
    ids: HashMap<FuncIdentity, EnvId>,
    rows: IdxVec<EnvId, (TypedHandle<RecordType>, EnvFieldSpan)>,
    fields: Pool<EnvFieldSpan, EnvironmentField>,
}

impl Environments {
    pub(crate) fn id(&self, identity: FuncIdentity) -> Option<EnvId> {
        self.ids.get(&identity).copied()
    }

    pub(super) fn reserve(
        &mut self,
        identity: FuncIdentity,
        record: TypedHandle<RecordType>,
    ) -> EnvId {
        let id = self.rows.push((record, self.fields.extend([])));
        self.ids.insert(identity, id);
        id
    }

    pub(super) fn define(&mut self, id: EnvId, fields: Vec<EnvironmentField>) {
        self.rows[id].1 = self.fields.extend(fields);
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = EnvId> + use<> {
        self.rows.ids()
    }

    pub(crate) fn record(&self, id: EnvId) -> TypedHandle<RecordType> {
        self.rows[id].0
    }

    pub(crate) fn fields(&self, id: EnvId) -> &[EnvironmentField] {
        &self.fields[self.rows[id].1]
    }
}
