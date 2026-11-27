//! Local writes in lexical regions and their selected callees.
use super::*;
use crate::engine::BodyOwner;
use crate::narrowing::{Region, WriteEffects};
use std::collections::HashSet;

type Site = (Option<InstanceId>, Region);

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
        let mut pending: Vec<_> = Region::of(body, expressions)
            .map(|r| (Some(s.inst), r))
            .collect();
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
        let Some(indexed) = self.index.regions.get(&region) else {
            return;
        };
        let events = &self.index.events;
        let boundary = |site| events.partition_point(|&(key, _)| key < (indexed.template, site));
        for &(_, expr) in &events[boundary(indexed.sites.start)..boundary(indexed.sites.end)] {
            match expr {
                Expr::LocalSet(id, _) | Expr::Update { id, .. } => write(*id),
                Expr::Closure { func_id, .. } | Expr::FuncRef(func_id) => {
                    if let Some(t) = self.index.template_of_func(*func_id) {
                        call((None, Region::Body(self.index.view(t).body)));
                    }
                }
                _ => {}
            }
        }
        if let Some(inst) = owner {
            for (_, entry) in self.inst_sites[inst].range(indexed.sites.clone()) {
                self.for_each_site_target(
                    entry,
                    |id| {
                        let callee = self.index.view(self.instances[id].template);
                        call((Some(id), Region::Body(callee.body)));
                    },
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
        let mut callers = HashMap::<_, Vec<_>>::new();
        let mut pending = Vec::new();
        for (&region, indexed) in &self.index.regions {
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
        let mut writes = WriteEffects::new();
        for ((id, region), local) in seen {
            let owner = match id {
                Some(id) if id == init => BodyOwner::Initializer,
                Some(id) => BodyOwner::Function(self.instances[id].emitted_func_id),
                None => BodyOwner::Function(
                    self.index
                        .view(self.index.regions[&region].template)
                        .func_id,
                ),
            };
            writes.entry((owner, region)).or_default().push(local);
        }
        writes
    }
}
