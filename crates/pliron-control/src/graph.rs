//! Strongly connected components of a graph with dense node indices.
use pliron::graph::{
    ControlFlowGraph, HasLabel,
    traversals::region::{Scc, sccs_in_topological_order},
};

struct Adjacency<'a> {
    successors: &'a [Vec<usize>],
    predecessors: Vec<Vec<usize>>,
}

impl<'a> Adjacency<'a> {
    fn new(successors: &'a [Vec<usize>]) -> Self {
        let mut predecessors = vec![Vec::new(); successors.len()];
        for (source, targets) in successors.iter().enumerate() {
            for &target in targets {
                if let Some(sources) = predecessors.get_mut(target) {
                    sources.push(source);
                }
            }
        }
        Self {
            successors,
            predecessors,
        }
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
        self.successors[*node].len()
    }
    fn get_successor(&self, _: &Labels, node: &usize, index: usize) -> usize {
        self.successors[*node][index]
    }
    fn num_predecessors(&self, _: &Labels, node: &usize) -> usize {
        self.predecessors[*node].len()
    }
    /// The trait contract bounds `index` by [`Self::num_predecessors`], the length of the
    /// predecessor list of `node`.
    fn get_predecessor(&self, _: &Labels, node: &usize, index: usize) -> usize {
        self.predecessors[*node][index]
    }
    fn entry_node(&self, _: &Labels) -> Option<usize> {
        (!self.successors.is_empty()).then_some(0)
    }
    fn nodes<'a>(&'a self, _: &'a Labels) -> Box<dyn Iterator<Item = usize> + 'a> {
        Box::new(0..self.successors.len())
    }
}

/// Components in topological order. All successors must index a row of the graph.
pub fn components(successors: &[Vec<usize>]) -> Vec<Scc<usize>> {
    sccs_in_topological_order(&Labels, &Adjacency::new(successors))
}
