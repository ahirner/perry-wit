//! Local writes in lexical regions and their selected callees.
use super::*;
use crate::engine::BodyOwner;
use crate::engine::types::{RegionId, WriteSpan};
use std::collections::HashSet;

type Site = (Option<InstanceId>, RegionId);

/// Locals that the code of each region assigns, directly or in a selected callee, by the body
/// that runs it.
#[derive(Default)]
pub(crate) struct WriteEffects {
    regions: IdxVec<RegionId, WriteSpan>,
    writes: Pool<WriteSpan, (BodyOwner, LocalId)>,
}

impl WriteEffects {
    pub(crate) fn locals(
        &self,
        region: RegionId,
        owner: BodyOwner,
    ) -> impl Iterator<Item = LocalId> + '_ {
        let rows = &self.writes[self.regions[region]];
        let start = rows.partition_point(|row| row.0 < owner);
        rows.iter()
            .skip(start)
            .take_while(move |row| row.0 == owner)
            .map(|row| row.1)
    }
}

impl<'h> Solver<'h, '_> {
    pub(super) fn invalidate_writes(
        &self,
        s: &mut EvalState<'h>,
        body: &'h [Stmt],
        expressions: impl IntoIterator<Item = &'h Expr>,
    ) {
        if s.guards.depth() == 0 {
            return;
        }
        let index = self.index;
        let mut pending = Vec::new();
        index.regions.of(
            &index.locals,
            &index.expressions,
            body,
            expressions,
            &mut |region| pending.push((Some(s.inst), region)),
        );
        let mut visited = HashSet::new();
        while let Some(site) = pending.pop() {
            if visited.insert(site) {
                self.visit_writes(
                    site,
                    |id| s.guards.assign(id),
                    |callee| pending.push(callee),
                );
            }
        }
    }

    fn visit_writes(
        &self,
        (owner, region): Site,
        mut write: impl FnMut(LocalId),
        mut call: impl FnMut(Site),
    ) {
        let index = self.index;
        let indexed = &index.regions.rows[region];
        let mut callee = |instance, template| {
            let body = index.view(template).body;
            let found = &mut |region| call((instance, region));
            index
                .regions
                .of(&index.locals, &index.expressions, body, [], found);
        };
        let events = &index.events;
        let boundary = |site| events.partition_point(|&(key, _)| key < (indexed.template, site));
        for &(_, expr) in &events[boundary(indexed.sites.start)..boundary(indexed.sites.end)] {
            match expr {
                Expr::LocalSet(id, _) | Expr::Update { id, .. } => write(*id),
                Expr::Closure { func_id, .. } | Expr::FuncRef(func_id) => {
                    if let Some(t) = index.template_of_func(*func_id) {
                        callee(None, t);
                    }
                }
                _ => {}
            }
        }
        if let Some(inst) = owner {
            for (_, entry) in self.inst_sites[inst].range(indexed.sites.clone()) {
                self.for_each_site_target(
                    entry,
                    |id| callee(Some(id), self.instances[id].template),
                    |_| {},
                );
            }
        }
    }

    pub(super) fn write_effects(&self, init: InstanceId, live: &BitSet) -> WriteEffects {
        let mut owners = self.index.views.filled(vec![None]);
        for (id, inst) in self.instances.iter_enumerated() {
            if live.contains(id.index()) {
                owners[inst.template].push(Some(id));
            }
        }
        let regions = &self.index.regions.rows;
        let mut callers = HashMap::<_, Vec<_>>::new();
        let mut pending = Vec::new();
        for (region, indexed) in regions.iter_enumerated() {
            for &id in &owners[indexed.template] {
                let site = (id, region);
                self.visit_writes(
                    site,
                    |local| pending.push((site, local)),
                    |callee| callers.entry(callee).or_default().push(site),
                );
            }
        }
        let mut seen = HashSet::new();
        while let Some((site, local)) = pending.pop() {
            if seen.insert((site, local))
                && let Some(parents) = callers.get(&site)
            {
                pending.extend(parents.iter().map(|&parent| (parent, local)));
            }
        }
        let mut rows: Vec<_> = seen
            .into_iter()
            .map(|((id, region), local)| {
                let owner = match id {
                    Some(id) if id == init => BodyOwner::Initializer,
                    Some(id) => BodyOwner::Function(self.instances[id].emitted_func_id),
                    None => BodyOwner::Function(self.index.view(regions[region].template).func_id),
                };
                (region, owner, local)
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        let mut rows = rows.into_iter().peekable();
        let mut effects = WriteEffects::default();
        for region in regions.ids() {
            let span = effects.writes.extend(std::iter::from_fn(|| {
                let (_, owner, local) = rows.next_if(|row| row.0 == region)?;
                Some((owner, local))
            }));
            effects.regions.push(span);
        }
        effects
    }
}
