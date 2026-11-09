//! Facts a branch condition proves about the values read in each arm, recovered from source
//! guards because Perry HIR carries no flow-sensitive types.
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Stmt, UnaryOp},
    types::LocalId,
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
        let defined_when = match op {
            CompareOp::Ne => true,
            CompareOp::Eq => false,
            _ => return None,
        };
        let tested = match (&**left, &**right) {
            (tested, Expr::Undefined) | (Expr::Undefined, tested) => tested,
            _ => return None,
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

struct Guard<'s> {
    path: Path<'s>,
    tag: &'s str,
    equal: bool,
    discriminant: Discriminant,
    killed: bool,
}

/// The tag guards in effect, appended as conditions narrow and truncated when branches exit.
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

    fn push(&mut self, path: Path<'s>, tag: &'s str, equal: bool, discriminant: Discriminant) {
        self.stack.push(Guard {
            path,
            tag,
            equal,
            discriminant,
            killed: false,
        });
    }

    /// The guards in effect for `path`: each tag and whether the value equals it.
    pub fn tags<'a>(
        &'a self,
        path: &'a Path<'s>,
        discriminant: Discriminant,
    ) -> impl Iterator<Item = (&'s str, bool)> + 'a {
        self.stack
            .iter()
            .filter(move |guard| {
                !guard.killed && &guard.path == path && guard.discriminant == discriminant
            })
            .map(|guard| (guard.tag, guard.equal))
    }

    /// Record the guards `condition` establishes where it evaluates to `truth`.
    pub fn condition(&mut self, condition: &'s Expr, truth: bool) {
        match condition {
            Expr::Unary {
                op: UnaryOp::Not,
                operand,
            } => self.condition(operand, !truth),
            Expr::Logical { op, left, right } => {
                if matches!((op, truth), (LogicalOp::And, true) | (LogicalOp::Or, false)) {
                    self.condition(left, truth);
                    self.condition(right, truth);
                }
            }
            Expr::Compare {
                op: CompareOp::Eq,
                left,
                right,
            } => self.equality(left, right, truth),
            Expr::Compare {
                op: CompareOp::Ne,
                left,
                right,
            } => self.equality(left, right, !truth),
            _ => {}
        }
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
            self.push(path, tag.as_str(), equal, discriminant);
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
        self.push(Path::local(local), tag, true, discriminant);
    }

    /// End the guards on every local a loop or callback body assigns: its later stores
    /// reach its start on the next iteration. Closures written in the body or bound to a local
    /// it reads (`closure`) count, since lowering inlines their calls.
    pub fn assign_in<'e>(
        &mut self,
        statements: &[Stmt],
        expressions: impl IntoIterator<Item = &'e Expr>,
        closure: impl Fn(LocalId) -> Option<&'e Expr>,
    ) {
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

    /// A mark for [`Self::revive`].
    pub fn mark(&self) -> usize {
        self.kills.len()
    }

    /// Undo the kills since `mark`, for a branch arm that does not reach the join, and return
    /// them so that [`Self::kill`] can apply them again. Scopes nest: every truncation inside
    /// the arm keeps the guards that existed at `mark`, so the kills before it and the indices
    /// after it stay valid.
    pub fn revive(&mut self, mark: usize) -> Vec<usize> {
        let revived = self.kills.split_off(mark);
        for &index in &revived {
            self.stack[index].killed = false;
        }
        revived
    }

    /// Apply kills returned by [`Self::revive`] again, for an arm that reaches the join.
    pub fn kill(&mut self, indices: Vec<usize>) {
        for index in indices {
            if !self.stack[index].killed {
                self.stack[index].killed = true;
                self.kills.push(index);
            }
        }
    }
}

/// The locals that statements may assign, entering closures they define or read.
struct Assignments<F> {
    closure: F,
    /// Bound closures already scanned, which also ends recursion through self-reference.
    entered: Vec<LocalId>,
    assigned: Vec<LocalId>,
}

impl<'e, F: Fn(LocalId) -> Option<&'e Expr>> Assignments<F> {
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
            Expr::LocalGet(id) if !self.entered.contains(id) => {
                if let Some(Expr::Closure { body, .. }) = (self.closure)(*id) {
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
