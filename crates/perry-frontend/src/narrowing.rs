//! Facts a branch condition proves about the values read in each arm, recovered from source
//! guards because Perry HIR carries no flow-sensitive types.
use crate::Absence;
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, UnaryOp},
    types::LocalId,
};

/// Access path rooted at a local through property reads, such as `x` or `x.a.b`.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct Path<'s>(LocalId, Vec<&'s str>);

impl<'s> Path<'s> {
    /// The path of the local itself.
    pub(crate) fn local(id: LocalId) -> Self {
        Self(id, Vec::new())
    }

    /// The local the path starts at.
    pub(crate) fn root(&self) -> LocalId {
        self.0
    }
}

/// The access path `expr` reads, or `None` when it is not a local or a property chain on one.
pub(crate) fn path(expr: &Expr) -> Option<Path<'_>> {
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
pub(crate) struct DefinedGuard<'s> {
    path: Path<'s>,
    defined_when: bool,
}

impl<'s> DefinedGuard<'s> {
    /// The guard `condition` establishes, if it is `p === undefined` or `p !== undefined`
    /// (either operand order) for an access path `p`.
    pub(crate) fn of(condition: &'s Expr) -> Option<Self> {
        let Expr::Compare {
            op, left, right, ..
        } = condition
        else {
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
    pub(crate) fn proves(&self, truth: bool, expr: &Expr) -> bool {
        self.defined_when == truth && path(expr).is_some_and(|path| path == self.path)
    }
}

/// What a narrowing guard compares with a string literal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Discriminant {
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
        Expr::Logical {
            op, left, right, ..
        } if matches!((op, truth), (LogicalOp::And, true) | (LogicalOp::Or, false)) => {
            comparisons(left, truth, visit);
            comparisons(right, truth, visit);
        }
        Expr::Compare {
            op, left, right, ..
        } => visit(*op, left, right, truth),
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
        Expr::Logical {
            op, left, right, ..
        } if matches!((op, truth), (LogicalOp::And, true) | (LogicalOp::Or, false)) => {
            truthy_locals(left, truth, visit);
            truthy_locals(right, truth, visit);
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Branch {
    depth: usize,
    kills: usize,
}

/// The guards in effect, appended as conditions narrow and truncated when branches exit.
/// An assignment kills the guards on its local in place, so lengths stay valid scope marks.
#[derive(Default)]
pub(crate) struct Guards<'s> {
    stack: Vec<Guard<'s>>,
    /// Stack indices in kill order, so a branch arm can undo the kills it made.
    kills: Vec<usize>,
}

impl<'s> Guards<'s> {
    /// The scope mark that [`Self::truncate`] returns to.
    pub(crate) fn depth(&self) -> usize {
        self.stack.len()
    }

    pub(crate) fn truncate(&mut self, len: usize) {
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

    fn facts<'a>(&'a self, path: &'a Path<'s>) -> impl Iterator<Item = Fact<'s>> + Clone + 'a {
        self.stack
            .iter()
            .filter(move |g| !g.killed && &g.path == path)
            .map(|g| g.fact)
    }

    pub(crate) fn tags<'a>(
        &'a self,
        path: &'a Path<'s>,
        discriminant: Discriminant,
    ) -> impl Iterator<Item = (&'s str, bool)> + Clone + 'a {
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
    }

    /// Record the tag guards `condition` establishes where it evaluates to `truth`.
    pub(crate) fn condition(&mut self, condition: &'s Expr, truth: bool) {
        comparisons(condition, truth, &mut |op, left, right, truth| match op {
            CompareOp::Eq => self.equality(left, right, truth),
            CompareOp::Ne => self.equality(left, right, !truth),
            _ => {}
        });
    }

    /// Record that `left === right` is `equal` when one side is a tag or `typeof` of a path
    /// and the other a string.
    pub(crate) fn equality(&mut self, left: &'s Expr, right: &'s Expr, equal: bool) {
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
    pub(crate) fn assign(&mut self, local: LocalId) {
        for (index, guard) in self.stack.iter_mut().enumerate() {
            if !guard.killed && guard.path.root() == local {
                guard.killed = true;
                self.kills.push(index);
            }
        }
    }

    /// After an assignment, `local` holds the alternative whose `discriminant` is `tag`.
    pub(crate) fn assign_as(&mut self, local: LocalId, discriminant: Discriminant, tag: &'s str) {
        self.assign(local);
        self.push(Path::local(local), Fact::Tag(discriminant, tag, true));
    }

    pub(crate) fn branch(&self) -> Branch {
        Branch {
            depth: self.depth(),
            kills: self.kills.len(),
        }
    }

    pub(crate) fn other_arm(&mut self, branch: Branch) -> Vec<usize> {
        self.truncate(branch.depth);
        self.revive(branch.kills)
    }

    /// Keep writes from arms reaching the join. A single surviving arm retains its condition.
    pub(crate) fn join(
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
