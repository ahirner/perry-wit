//! Facts a branch condition proves about the values read in each arm, recovered from source
//! guards because Perry HIR carries no flow-sensitive types.
use crate::Absence;
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Stmt, UnaryOp},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};

/// Access path rooted at a local through property reads, such as `x` or `x.a.b`.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Path<'s>(LocalId, Vec<&'s str>);

impl<'s> Path<'s> {
    /// The path of the local itself.
    pub fn local(id: LocalId) -> Self {
        Self(id, Vec::new())
    }

    /// The local the path starts at.
    pub fn root(&self) -> LocalId {
        self.0
    }
}

/// The access path `expr` reads, or `None` when it is not a local or a property chain on one.
pub fn path(expr: &Expr) -> Option<Path<'_>> {
    match expr {
        Expr::LocalGet(id) => Some(Path(*id, vec![])),
        Expr::PropertyGet {
            object, property, ..
        } => {
            let mut path = path(object)?;
            path.1.push(property.as_str());
            Some(path)
        }
        _ => None,
    }
}

/// A strict comparison of an access path with `undefined`: the path is defined in the arm
/// where the condition evaluates to `defined_when`.
#[derive(Debug)]
pub struct DefinedGuard<'s> {
    path: Path<'s>,
    defined_when: bool,
}

impl<'s> DefinedGuard<'s> {
    /// The guard `condition` establishes, if it is `p === undefined` or `p !== undefined`
    /// (either operand order) for an access path `p`.
    pub fn of(condition: &'s Expr) -> Option<Self> {
        let Expr::Compare { op, left, right } = condition else {
            return None;
        };
        let (tested, Absence::Undefined, defined_when) = absence_test(*op, left, right)? else {
            return None;
        };
        Some(Self {
            path: path(tested)?,
            defined_when,
        })
    }

    /// Whether `expr`, evaluated in the arm where the condition is `truth`, reads the guarded
    /// path and is therefore not `undefined`.
    pub fn proves(&self, truth: bool, expr: &Expr) -> bool {
        self.defined_when == truth && path(expr).is_some_and(|path| path == self.path)
    }
}

/// What a narrowing guard compares with a string literal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Discriminant {
    TypeOf,
    Tag,
}

#[derive(Clone, Copy)]
enum Fact<'s> {
    Tag(Discriminant, &'s str, bool),
    Excludes(Absence),
}

struct Guard<'s> {
    path: Path<'s>,
    fact: Fact<'s>,
    killed: bool,
}

fn absence_test<'a>(
    op: CompareOp,
    left: &'a Expr,
    right: &'a Expr,
) -> Option<(&'a Expr, Absence, bool)> {
    let (value, state) = match (left, right) {
        (value, Expr::Null) | (Expr::Null, value) => (value, Absence::Null),
        (value, Expr::Undefined) | (Expr::Undefined, value) => (value, Absence::Undefined),
        _ => return None,
    };
    Some(match op {
        CompareOp::Eq => (value, state, false),
        CompareOp::Ne => (value, state, true),
        CompareOp::LooseEq => (value, Absence::Both, false),
        CompareOp::LooseNe => (value, Absence::Both, true),
        _ => return None,
    })
}

fn comparisons<'s>(
    condition: &'s Expr,
    truth: bool,
    visit: &mut impl FnMut(CompareOp, &'s Expr, &'s Expr, bool),
) {
    match condition {
        Expr::Sequence(parts) => {
            if let Some(last) = parts.last() {
                comparisons(last, truth, visit);
            }
        }
        Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } => comparisons(operand, !truth, visit),
        Expr::Logical { op, left, right }
            if matches!((op, truth), (LogicalOp::And, true) | (LogicalOp::Or, false)) =>
        {
            comparisons(left, truth, visit);
            comparisons(right, truth, visit);
        }
        Expr::Compare { op, left, right } => visit(*op, left, right, truth),
        _ => {}
    }
}

fn truthy_locals(condition: &Expr, truth: bool, visit: &mut impl FnMut(LocalId)) {
    match condition {
        Expr::Sequence(parts) => {
            if let Some(last) = parts.last() {
                truthy_locals(last, truth, visit);
            }
        }
        Expr::LocalGet(local) if truth => visit(*local),
        Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } => truthy_locals(operand, !truth, visit),
        Expr::Logical { op, left, right }
            if matches!((op, truth), (LogicalOp::And, true) | (LogicalOp::Or, false)) =>
        {
            truthy_locals(left, truth, visit);
            truthy_locals(right, truth, visit);
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
pub struct Branch {
    depth: usize,
    kills: usize,
}

/// The guards in effect, appended as conditions narrow and truncated when branches exit.
/// An assignment kills the guards on its local in place, so lengths stay valid scope marks.
#[derive(Default)]
pub struct Guards<'s> {
    stack: Vec<Guard<'s>>,
    /// Stack indices in kill order, so a branch arm can undo the kills it made.
    kills: Vec<usize>,
}

impl<'s> Guards<'s> {
    /// The scope mark that [`Self::truncate`] returns to.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn truncate(&mut self, len: usize) {
        self.stack.truncate(len);
        self.kills.retain(|&index| index < len);
    }

    fn push(&mut self, path: Path<'s>, fact: Fact<'s>) {
        self.stack.push(Guard {
            path,
            fact,
            killed: false,
        });
    }

    fn facts<'a>(&'a self, path: &'a Path<'s>) -> impl Iterator<Item = Fact<'s>> + 'a {
        self.stack
            .iter()
            .filter(move |g| !g.killed && &g.path == path)
            .map(|g| g.fact)
    }

    pub fn tags<'a>(
        &'a self,
        path: &'a Path<'s>,
        discriminant: Discriminant,
    ) -> impl Iterator<Item = (&'s str, bool)> + 'a {
        self.facts(path).filter_map(move |fact| match fact {
            Fact::Tag(d, tag, equal) if d == discriminant => Some((tag, equal)),
            _ => None,
        })
    }

    pub(crate) fn excluded(&self, id: LocalId) -> Option<Absence> {
        self.facts(&Path::local(id))
            .filter_map(|fact| match fact {
                Fact::Excludes(state) => Some(state),
                _ => None,
            })
            .reduce(Absence::join)
    }

    pub(crate) fn exclude_absences(&mut self, condition: &Expr, truth: bool) {
        comparisons(condition, truth, &mut |op, left, right, truth| {
            if let Some((Expr::LocalGet(local), absence, excluded_when)) =
                absence_test(op, left, right)
                && truth == excluded_when
            {
                self.push(Path::local(*local), Fact::Excludes(absence));
            }
        });
        truthy_locals(condition, truth, &mut |local| {
            self.push(Path::local(local), Fact::Excludes(Absence::Both));
        });
        // A later operand may write a value tested by an earlier operand.
        if self.depth() != 0 {
            self.assign_in(&[], [condition], |_| None);
        }
    }

    /// Record the tag guards `condition` establishes where it evaluates to `truth`.
    pub fn condition(&mut self, condition: &'s Expr, truth: bool) {
        comparisons(condition, truth, &mut |op, left, right, truth| match op {
            CompareOp::Eq => self.equality(left, right, truth),
            CompareOp::Ne => self.equality(left, right, !truth),
            _ => {}
        });
    }

    /// Record that `left === right` is `equal` when one side is a tag or `typeof` of a path
    /// and the other a string.
    pub fn equality(&mut self, left: &'s Expr, right: &'s Expr, equal: bool) {
        let (object, tag, discriminant) = match (left, right) {
            (
                Expr::PropertyGet {
                    object, property, ..
                },
                Expr::String(tag),
            )
            | (
                Expr::String(tag),
                Expr::PropertyGet {
                    object, property, ..
                },
            ) if property == "tag" => (object, tag, Discriminant::Tag),
            (Expr::TypeOf(object), Expr::String(tag))
            | (Expr::String(tag), Expr::TypeOf(object)) => (object, tag, Discriminant::TypeOf),
            _ => return,
        };
        if let Some(path) = path(object) {
            self.push(path, Fact::Tag(discriminant, tag.as_str(), equal));
        }
    }

    /// End the guards on `local`, which is assigned or bound again.
    pub fn assign(&mut self, local: LocalId) {
        for (index, guard) in self.stack.iter_mut().enumerate() {
            if !guard.killed && guard.path.root() == local {
                guard.killed = true;
                self.kills.push(index);
            }
        }
    }

    /// After an assignment, `local` holds the alternative whose `discriminant` is `tag`.
    pub fn assign_as(&mut self, local: LocalId, discriminant: Discriminant, tag: &'s str) {
        self.assign(local);
        self.push(Path::local(local), Fact::Tag(discriminant, tag, true));
    }

    /// End the guards on every local a loop or callback body assigns: its later stores
    /// reach its start on the next iteration. Include writes in statically called closures.
    pub fn assign_in<'e>(
        &mut self,
        statements: &[Stmt],
        expressions: impl IntoIterator<Item = &'e Expr>,
        closure: impl Fn(FuncId) -> Option<&'e [Stmt]>,
    ) {
        if self.stack.is_empty() {
            return;
        }
        let mut scan = Assignments {
            closure,
            entered: Vec::new(),
            assigned: Vec::new(),
        };
        scan.statements(statements);
        for expr in expressions {
            scan.expression(expr);
        }
        for local in scan.assigned {
            self.assign(local);
        }
    }

    pub fn branch(&self) -> Branch {
        Branch {
            depth: self.depth(),
            kills: self.kills.len(),
        }
    }

    pub fn other_arm(&mut self, branch: Branch) -> Vec<usize> {
        self.truncate(branch.depth);
        self.revive(branch.kills)
    }

    /// Keep writes from arms reaching the join. A single surviving arm retains its condition.
    pub fn join(
        &mut self,
        branch: Branch,
        then_writes: Vec<usize>,
        then_reaches: bool,
        else_reaches: bool,
    ) -> Option<bool> {
        self.truncate(branch.depth);
        if !else_reaches {
            self.revive(branch.kills);
        }
        if then_reaches {
            for index in then_writes {
                if let Some(guard) = self.stack.get_mut(index)
                    && !guard.killed
                {
                    guard.killed = true;
                    self.kills.push(index);
                }
            }
        }
        (then_reaches != else_reaches).then_some(then_reaches)
    }

    fn revive(&mut self, mark: usize) -> Vec<usize> {
        let revived = self.kills.split_off(mark);
        for &index in &revived {
            if let Some(guard) = self.stack.get_mut(index) {
                guard.killed = false;
            }
        }
        revived
    }
}

/// The locals that statements may assign, entering closures they define or read.
struct Assignments<F> {
    closure: F,
    /// Bound closures already scanned, which also ends recursion through self-reference.
    entered: Vec<FuncId>,
    assigned: Vec<LocalId>,
}

impl<'e, F: Fn(FuncId) -> Option<&'e [Stmt]>> Assignments<F> {
    fn statements(&mut self, statements: &[Stmt]) {
        for statement in statements {
            stmt_any_expr(statement, &mut |expr| {
                self.expression(expr);
                false
            });
        }
    }

    fn expression(&mut self, expr: &Expr) {
        match expr {
            Expr::LocalSet(id, _) | Expr::Update { id, .. } => self.assigned.push(*id),
            Expr::FuncRef(id) => {
                if !self.entered.contains(id)
                    && let Some(body) = (self.closure)(*id)
                {
                    self.entered.push(*id);
                    self.statements(body);
                }
            }
            Expr::Closure { body, .. } => self.statements(body),
            _ => {}
        }
        walk_expr_children(expr, &mut |child| self.expression(child));
    }
}
