//! Strongly connected components of a graph with dense node indices.
use pliron::graph::{
    ControlFlowGraph, HasLabel,
    traversals::region::{Scc, sccs_in_topological_order},
};

struct Adjacency<'a>(&'a [Vec<usize>]);

impl Adjacency<'_> {
    // Tarjan follows only successors, so reverse edges need no stored index.
    fn incoming(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        self.0
            .iter()
            .enumerate()
            .flat_map(move |(source, targets)| {
                targets
                    .iter()
                    .filter_map(move |&target| (target == node).then_some(source))
            })
    }
}

struct Labels;
impl HasLabel<Labels> for usize {
    fn label(&self, _: &Labels) -> String {
        self.to_string()
    }
}

impl ControlFlowGraph<Labels> for Adjacency<'_> {
    type Node = usize;
    fn num_successors(&self, _: &Labels, node: &usize) -> usize {
        self.0[*node].len()
    }
    fn get_successor(&self, _: &Labels, node: &usize, index: usize) -> usize {
        self.0[*node][index]
    }
    // pliron's Tarjan SCC walks successors only. These scans answer the trait lazily.
    fn num_predecessors(&self, _: &Labels, node: &usize) -> usize {
        self.incoming(*node).count()
    }
    fn get_predecessor(&self, _: &Labels, node: &usize, index: usize) -> usize {
        self.incoming(*node)
            .nth(index)
            .expect("predecessor index out of bounds")
    }
    fn entry_node(&self, _: &Labels) -> Option<usize> {
        (!self.0.is_empty()).then_some(0)
    }
    fn nodes<'a>(&'a self, _: &'a Labels) -> Box<dyn Iterator<Item = usize> + 'a> {
        Box::new(0..self.0.len())
    }
}

/// Components in topological order. All successors must index a row of the graph.
pub fn components(successors: &[Vec<usize>]) -> Vec<Scc<usize>> {
    sccs_in_topological_order(&Labels, &Adjacency(successors))
}
