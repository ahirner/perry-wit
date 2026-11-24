//! Dense interned types over `shapes::TypeId`, canonical sorted unions, and monotone joins.
use super::bitset::BitSet;
use crate::{Absence, shapes};
use perry_hir::types::{ObjectType, PropertyInfo, Type as HirType};
use pliron::context::Context;
use std::collections::HashMap;

const MAX_TYPE_DEPTH: u8 = 16;

mod sealed {
    pub(crate) trait MintIdx: Copy + Eq {
        fn from_usize(n: usize) -> Self;
    }
}

pub(crate) trait Idx: Copy + Eq {
    fn index(self) -> usize;
}

/// Index-typed vector whose IDs have private fields and are minted only by [`IdxVec::push`].
pub(crate) struct IdxVec<I, T> {
    data: Vec<T>,
    _marker: std::marker::PhantomData<fn(I) -> I>,
}

impl<I, T> Default for IdxVec<I, T> {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<I, T: Clone> Clone for IdxVec<I, T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

#[allow(private_bounds)]
impl<I: Idx + sealed::MintIdx, T> IdxVec<I, T> {
    pub(crate) fn push(&mut self, value: T) -> I {
        let id = I::from_usize(self.data.len());
        self.data.push(value);
        id
    }
    pub(crate) fn get(&self, id: I) -> Option<&T> {
        self.data.get(id.index())
    }
    pub(crate) fn get_mut(&mut self, id: I) -> Option<&mut T> {
        self.data.get_mut(id.index())
    }
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }
    pub(crate) fn ids(&self) -> impl Iterator<Item = I> + use<I, T> {
        (0..self.len()).map(I::from_usize)
    }
    pub(crate) fn filled<U: Clone>(&self, value: U) -> IdxVec<I, U> {
        IdxVec {
            data: vec![value; self.len()],
            _marker: std::marker::PhantomData,
        }
    }
    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, T> {
        self.data.iter()
    }
    pub(crate) fn iter_enumerated(&self) -> impl Iterator<Item = (I, &T)> {
        self.data
            .iter()
            .enumerate()
            .map(|(i, v)| (I::from_usize(i), v))
    }
    pub(crate) fn as_slice(&self) -> &[T] {
        &self.data
    }
}

impl<I: Idx, T> std::ops::Index<I> for IdxVec<I, T> {
    type Output = T;
    fn index(&self, id: I) -> &T {
        debug_assert!(id.index() < self.data.len());
        &self.data[id.index()]
    }
}

impl<I: Idx, T> std::ops::IndexMut<I> for IdxVec<I, T> {
    fn index_mut(&mut self, id: I) -> &mut T {
        debug_assert!(id.index() < self.data.len());
        &mut self.data[id.index()]
    }
}

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
            pub(crate) struct $name(u32);
            impl $name {
                #[allow(dead_code)]
                pub(crate) fn index(self) -> usize { self.0 as usize }
            }
            impl Idx for $name {
                fn index(self) -> usize { self.0 as usize }
            }
            impl sealed::MintIdx for $name {
                fn from_usize(n: usize) -> Self {
                    debug_assert!(u32::try_from(n).is_ok(), "index table overflow");
                    Self(n as u32)
                }
            }
        )*
    };
}

id_type!(
    Ty,
    TemplateId,
    InstanceId,
    ClassId,
    ClassInstId,
    LocId,
    LocalIdx,
    NameId,
    HelperId,
);

impl TemplateId {
    pub(crate) const INIT: Self = Self(0);
    #[cfg(test)]
    pub(crate) fn test_id(n: u32) -> Self {
        Self(n)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct TySpan {
    pub(crate) start: u32,
    pub(crate) len: u32,
}
impl TySpan {
    pub(crate) fn range(self) -> std::ops::Range<usize> {
        let s = self.start as usize;
        s..s.saturating_add(self.len as usize)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) enum CaptureOwner {
    None,
    Instance(InstanceId),
    Template(TemplateId),
}

impl CaptureOwner {
    pub(crate) fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
    pub(crate) fn is_some(self) -> bool {
        !self.is_none()
    }
    pub(crate) fn instance(self) -> Option<InstanceId> {
        match self {
            Self::Instance(id) => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct FuncIdentity {
    pub(crate) template: TemplateId,
    pub(crate) capture_owner: CaptureOwner,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ObjectProp {
    pub(crate) name: NameId,
    pub(crate) ty: Ty,
    pub(crate) optional: bool,
    pub(crate) readonly: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CellKind {
    Array,
    LocalArray,
    Dict,
    Promise,
}
impl CellKind {
    pub(crate) fn is_array(self) -> bool {
        matches!(self, Self::Array | Self::LocalArray)
    }
    pub(crate) fn is_promise(self) -> bool {
        matches!(self, Self::Promise)
    }
    pub(crate) fn is_conservative(self) -> bool {
        matches!(self, Self::LocalArray | Self::Dict)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum TyKind {
    Bottom,
    Open,
    Dynamic,
    Never,
    Void,
    Null,
    Boolean,
    Number,
    Int32,
    BigInt,
    String,
    Symbol,
    Leaf(shapes::TypeId),
    TypeVar(shapes::TypeId),
    Array(Ty),
    Cell { loc: LocId, kind: CellKind },
    Promise(Ty),
    Tuple(TySpan),
    Union(TySpan),
    Generic { base: shapes::TypeId, args: TySpan },
    Object { props: TySpan, index: Option<Ty> },
    Func(FuncIdentity),
    ClassInst(ClassInstId),
}

impl Ty {
    pub(crate) const BOTTOM: Self = Self(0);
    pub(crate) const OPEN: Self = Self(1);
    pub(crate) const DYNAMIC: Self = Self(2);
    pub(crate) const NEVER: Self = Self(3);
    pub(crate) const VOID: Self = Self(4);
    pub(crate) const NULL: Self = Self(5);
    pub(crate) const BOOLEAN: Self = Self(6);
    pub(crate) const NUMBER: Self = Self(7);
    pub(crate) const INT32: Self = Self(8);
    pub(crate) const BIGINT: Self = Self(9);
    pub(crate) const STRING: Self = Self(10);
    pub(crate) const SYMBOL: Self = Self(11);
}

#[derive(Clone, Copy, Default)]
struct TyMeta {
    open: bool,
    depth: u8,
}

#[derive(Default)]
pub(crate) struct TypeTable {
    kinds: IdxVec<Ty, TyKind>,
    meta: IdxVec<Ty, TyMeta>,
    kind_index: HashMap<TyKind, Ty>,
    members_index: HashMap<Box<[Ty]>, TySpan>,
    props_index: HashMap<Box<[ObjectProp]>, TySpan>,
    ty_pool: Vec<Ty>,
    object_props: Vec<ObjectProp>,
    names: IdxVec<NameId, String>,
    name_index: HashMap<String, NameId>,
    scratch: Vec<Ty>,
}

impl TypeTable {
    pub(crate) fn new() -> Self {
        let mut table = Self {
            kind_index: HashMap::with_capacity(64),
            name_index: HashMap::with_capacity(32),
            ..Self::default()
        };
        for kind in [
            TyKind::Bottom,
            TyKind::Open,
            TyKind::Dynamic,
            TyKind::Never,
            TyKind::Void,
            TyKind::Null,
            TyKind::Boolean,
            TyKind::Number,
            TyKind::Int32,
            TyKind::BigInt,
            TyKind::String,
            TyKind::Symbol,
        ] {
            let id = table.push_kind(kind);
            table.kind_index.insert(kind, id);
        }
        table
    }

    fn push_kind(&mut self, kind: TyKind) -> Ty {
        let (open, depth) = match kind {
            TyKind::Bottom | TyKind::Open | TyKind::TypeVar(_) => (true, 0),
            TyKind::Array(i) | TyKind::Promise(i) => {
                let m = self.meta[i];
                (m.open, m.depth.saturating_add(1))
            }
            TyKind::Tuple(s) | TyKind::Generic { args: s, .. } => {
                let (mut o, mut d) = (false, 0u8);
                for &c in self.ty_slice(s) {
                    let m = self.meta[c];
                    o |= m.open;
                    d = d.max(m.depth);
                }
                (o, d.saturating_add(1))
            }
            TyKind::Union(s) => {
                let (mut o, mut d) = (false, 0u8);
                for &c in self.ty_slice(s) {
                    let m = self.meta[c];
                    o |= m.open;
                    d = d.max(m.depth);
                }
                (o, d)
            }
            TyKind::Object { props, index } => {
                let (mut o, mut d) = index.map_or((false, 0u8), |i| {
                    let m = self.meta[i];
                    (m.open, m.depth)
                });
                for p in self.object_slice(props) {
                    let m = self.meta[p.ty];
                    o |= m.open;
                    d = d.max(m.depth);
                }
                (o, d.saturating_add(1))
            }
            _ => (false, 0),
        };
        let ty = self.kinds.push(kind);
        let meta_ty = self.meta.push(TyMeta { open, depth });
        debug_assert_eq!(ty, meta_ty);
        ty
    }

    pub(crate) fn kind(&self, ty: Ty) -> TyKind {
        self.kinds[ty]
    }
    pub(crate) fn ty_slice(&self, span: TySpan) -> &[Ty] {
        debug_assert!(span.range().end <= self.ty_pool.len());
        self.ty_pool.get(span.range()).unwrap_or(&[])
    }
    pub(crate) fn tuple_slice(&self, ty: Ty) -> &[Ty] {
        match self.kind(ty) {
            TyKind::Tuple(span) => self.ty_slice(span),
            _ => &[],
        }
    }
    pub(crate) fn object_slice(&self, span: TySpan) -> &[ObjectProp] {
        debug_assert!(span.range().end <= self.object_props.len());
        self.object_props.get(span.range()).unwrap_or(&[])
    }
    pub(crate) fn name_str(&self, id: NameId) -> &str {
        self.names[id].as_str()
    }

    pub(crate) fn intern_name(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.name_index.get(name) {
            return id;
        }
        let owned = name.to_owned();
        let id = self.names.push(owned.clone());
        self.name_index.insert(owned, id);
        id
    }

    fn intern_span<T: Copy + Eq + std::hash::Hash>(
        index: &mut HashMap<Box<[T]>, TySpan>,
        pool: &mut Vec<T>,
        items: &[T],
    ) -> TySpan {
        if let Some(&span) = index.get(items) {
            return span;
        }
        let span = TySpan {
            start: pool.len() as u32,
            len: items.len() as u32,
        };
        pool.extend_from_slice(items);
        index.insert(items.into(), span);
        span
    }

    fn intern_kind(&mut self, kind: TyKind) -> Ty {
        if let Some(&ty) = self.kind_index.get(&kind) {
            return ty;
        }
        let ty = self.push_kind(kind);
        self.kind_index.insert(kind, ty);
        ty
    }
    pub(crate) fn leaf(&mut self, id: shapes::TypeId) -> Ty {
        self.intern_kind(TyKind::Leaf(id))
    }
    pub(crate) fn named(&mut self, ctx: &Context, name: &str) -> Ty {
        self.leaf(shapes::TypeId::from_owned(
            ctx,
            HirType::Named(name.to_owned()),
        ))
    }
    pub(crate) fn type_var(&mut self, id: shapes::TypeId) -> Ty {
        self.intern_kind(TyKind::TypeVar(id))
    }
    pub(crate) fn array(&mut self, element: Ty) -> Ty {
        self.intern_kind(TyKind::Array(element))
    }
    pub(crate) fn cell(&mut self, loc: LocId, kind: CellKind) -> Ty {
        self.intern_kind(TyKind::Cell { loc, kind })
    }
    pub(crate) fn promise(&mut self, inner: Ty) -> Ty {
        self.intern_kind(TyKind::Promise(inner))
    }
    pub(crate) fn func(&mut self, identity: FuncIdentity) -> Ty {
        self.intern_kind(TyKind::Func(identity))
    }
    pub(crate) fn class_inst(&mut self, inst: ClassInstId) -> Ty {
        self.intern_kind(TyKind::ClassInst(inst))
    }

    pub(crate) fn tuple(&mut self, items: &[Ty]) -> Ty {
        let span = Self::intern_span(&mut self.members_index, &mut self.ty_pool, items);
        self.intern_kind(TyKind::Tuple(span))
    }
    pub(crate) fn generic(&mut self, base: shapes::TypeId, args: &[Ty]) -> Ty {
        let args = Self::intern_span(&mut self.members_index, &mut self.ty_pool, args);
        self.intern_kind(TyKind::Generic { base, args })
    }
    pub(crate) fn object(&mut self, props: &[ObjectProp], index: Option<Ty>) -> Ty {
        let span = Self::intern_span(&mut self.props_index, &mut self.object_props, props);
        self.intern_kind(TyKind::Object { props: span, index })
    }

    pub(crate) fn union(&mut self, members: &[Ty]) -> Ty {
        let mut flat = std::mem::take(&mut self.scratch);
        flat.clear();
        for &m in members {
            self.push_union_member(&mut flat, m);
        }
        let res = if flat.contains(&Ty::OPEN) {
            Ty::OPEN
        } else if flat.contains(&Ty::DYNAMIC) {
            Ty::DYNAMIC
        } else {
            flat.sort_unstable();
            flat.dedup();
            self.intern_sorted_union(&flat)
        };
        self.scratch = flat;
        res
    }

    fn push_union_member(&self, out: &mut Vec<Ty>, ty: Ty) {
        if ty == Ty::BOTTOM || ty == Ty::NEVER {
            return;
        }
        if let TyKind::Union(span) = self.kind(ty) {
            out.extend_from_slice(self.ty_slice(span));
        } else {
            out.push(ty);
        }
    }

    fn intern_sorted_union(&mut self, sorted: &[Ty]) -> Ty {
        match sorted {
            [] => Ty::NEVER,
            [single] => *single,
            slice => {
                let span = Self::intern_span(&mut self.members_index, &mut self.ty_pool, slice);
                self.intern_kind(TyKind::Union(span))
            }
        }
    }

    pub(crate) fn join(&mut self, a: Ty, b: Ty) -> Ty {
        if a == b || b == Ty::BOTTOM {
            a
        } else if a == Ty::BOTTOM {
            b
        } else if b == Ty::NEVER {
            a
        } else if a == Ty::NEVER {
            b
        } else if a == Ty::OPEN || b == Ty::OPEN {
            Ty::OPEN
        } else if a == Ty::DYNAMIC || b == Ty::DYNAMIC {
            Ty::DYNAMIC
        } else {
            self.union(&[a, b])
        }
    }

    fn is_composite(kind: TyKind) -> bool {
        matches!(
            kind,
            TyKind::Array(_)
                | TyKind::Object { .. }
                | TyKind::Tuple(_)
                | TyKind::Generic { .. }
                | TyKind::Promise(_)
        )
    }

    pub(crate) fn join_bounded(&mut self, old: Ty, value: Ty) -> (Ty, bool) {
        if old == value || value == Ty::BOTTOM || (value == Ty::NEVER && old != Ty::BOTTOM) {
            return (old, false);
        }
        let self_nested = match self.kind(old) {
            TyKind::Union(s) => self
                .ty_slice(s)
                .iter()
                .any(|&m| Self::is_composite(self.kind(m)) && self.contains_nested_ty(value, m)),
            k if Self::is_composite(k) => self.contains_nested_ty(value, old),
            _ => false,
        };
        if self_nested {
            return (Ty::OPEN, true);
        }
        let joined = self.join(old, value);
        if self.meta[joined].depth > MAX_TYPE_DEPTH {
            (Ty::OPEN, false)
        } else {
            (joined, false)
        }
    }

    fn normalize_atom(&self, ctx: &Context, decls: &shapes::Declarations<'_>, ty: Ty) -> Ty {
        match self.kind(ty) {
            TyKind::Int32 => Ty::NUMBER,
            TyKind::Leaf(id) => match id.hir(ctx) {
                HirType::StringLiteral(_) => Ty::STRING,
                HirType::Named(n) if matches!(decls.integer_carrier(n), Some(HirType::Number)) => {
                    Ty::NUMBER
                }
                _ => ty,
            },
            _ => ty,
        }
    }

    pub(crate) fn normalize_for_storage(
        &mut self,
        ctx: &Context,
        decls: &shapes::Declarations<'_>,
        ty: Ty,
    ) -> Ty {
        let TyKind::Union(span) = self.kind(ty) else {
            return self.normalize_atom(ctx, decls, ty);
        };
        let mut flat = std::mem::take(&mut self.scratch);
        flat.clear();
        for &m in self.ty_slice(span) {
            let n = self.normalize_atom(ctx, decls, m);
            if n != Ty::BOTTOM && n != Ty::NEVER {
                flat.push(n);
            }
        }
        flat.sort_unstable();
        flat.dedup();
        let res = self.intern_sorted_union(&flat);
        self.scratch = flat;
        res
    }

    pub(crate) fn strip_nullish(&mut self, ty: Ty) -> (Ty, Option<Absence>) {
        match self.kind(ty) {
            TyKind::Null => (Ty::VOID, Some(Absence::Null)),
            TyKind::Void => (Ty::VOID, Some(Absence::Undefined)),
            TyKind::Union(span) => {
                let mut defined = std::mem::take(&mut self.scratch);
                defined.clear();
                let mut abs = None;
                for &m in self.ty_slice(span) {
                    match m {
                        Ty::NULL => abs = Absence::join_optional(abs, Some(Absence::Null)),
                        Ty::VOID => abs = Absence::join_optional(abs, Some(Absence::Undefined)),
                        other => defined.push(other),
                    }
                }
                let inner = if defined.is_empty() {
                    Ty::VOID
                } else {
                    self.intern_sorted_union(&defined)
                };
                self.scratch = defined;
                (inner, abs)
            }
            _ => (ty, None),
        }
    }

    pub(crate) fn with_absences(&mut self, ty: Ty, states: impl Into<Option<Absence>>) -> Ty {
        let Some(states) = states.into() else {
            return ty;
        };
        let (inner, existing) = self.strip_nullish(ty);
        if matches!(inner, Ty::OPEN | Ty::DYNAMIC | Ty::VOID | Ty::BOTTOM) {
            return inner;
        }
        let states = existing.map_or(states, |e| e.join(states));
        let mut members = Vec::with_capacity(4);
        self.push_union_member(&mut members, inner);
        if states.admits_null() {
            members.push(Ty::NULL);
        }
        if states.admits_undefined() {
            members.push(Ty::VOID);
        }
        members.sort_unstable();
        members.dedup();
        self.intern_sorted_union(&members)
    }

    pub(crate) fn exclude_absences(&mut self, ty: Ty, excluded: Absence) -> Ty {
        let TyKind::Union(span) = self.kind(ty) else {
            return match ty {
                Ty::NULL if excluded.admits_null() => Ty::NEVER,
                Ty::VOID if excluded.admits_undefined() => Ty::NEVER,
                _ => ty,
            };
        };
        let mut kept = std::mem::take(&mut self.scratch);
        kept.clear();
        for &m in self.ty_slice(span) {
            if !((m == Ty::NULL && excluded.admits_null())
                || (m == Ty::VOID && excluded.admits_undefined()))
            {
                kept.push(m);
            }
        }
        let res = self.intern_sorted_union(&kept);
        self.scratch = kept;
        res
    }

    fn any_child(&self, ty: Ty, f: &mut impl FnMut(Ty) -> bool) -> bool {
        match self.kind(ty) {
            TyKind::Array(inner) | TyKind::Promise(inner) => f(inner),
            TyKind::Tuple(s) | TyKind::Union(s) | TyKind::Generic { args: s, .. } => {
                self.ty_slice(s).iter().copied().any(f)
            }
            TyKind::Object { props, index } => {
                index.is_some_and(&mut *f) || self.object_slice(props).iter().map(|p| p.ty).any(f)
            }
            _ => false,
        }
    }

    pub(crate) fn is_open(&self, ty: Ty) -> bool {
        self.meta[ty].open
    }

    fn any_with_cells(
        &self,
        ty: Ty,
        locations: &[Ty],
        visiting: &mut Vec<LocId>,
        seen: &mut BitSet,
        on_cell: &impl Fn(LocId, CellKind, Ty) -> Option<bool>,
        on_leaf: &impl Fn(TyKind) -> bool,
    ) -> bool {
        let kind = self.kind(ty);
        if on_leaf(kind) {
            return true;
        }
        if let TyKind::Cell { loc, kind: ck } = kind {
            let Some(&elem) = locations.get(loc.index()) else {
                return on_cell(loc, ck, Ty::OPEN).unwrap_or(true);
            };
            if let Some(res) = on_cell(loc, ck, elem) {
                return res;
            }
            if visiting.contains(&loc) {
                return on_leaf(TyKind::Open);
            }
            visiting.push(loc);
            let hit = self.any_with_cells(elem, locations, visiting, seen, on_cell, on_leaf);
            visiting.pop();
            return hit;
        }
        if visiting.is_empty() && !seen.insert(ty.index()) {
            return false;
        }
        self.any_child(ty, &mut |c| {
            self.any_with_cells(c, locations, visiting, seen, on_cell, on_leaf)
        })
    }

    pub(crate) fn is_open_with(&self, ty: Ty, locations: &[Ty], dict_reads: &BitSet) -> bool {
        self.any_with_cells(
            ty,
            locations,
            &mut Vec::new(),
            &mut BitSet::default(),
            &|loc, ck, elem| match ck {
                CellKind::Dict => {
                    Some(!self.is_valid_dict_value(elem, dict_reads.contains(loc.index())))
                }
                CellKind::Promise => (elem == Ty::BOTTOM).then_some(false),
                _ => None,
            },
            &|k| matches!(k, TyKind::Bottom | TyKind::Open | TyKind::TypeVar(_)),
        )
    }

    pub(crate) fn is_promise_ty(&self, ty: Ty) -> bool {
        match self.kind(ty) {
            TyKind::Promise(_)
            | TyKind::Cell {
                kind: CellKind::Promise,
                ..
            } => true,
            TyKind::Union(span) => self.ty_slice(span).iter().any(|&m| self.is_promise_ty(m)),
            _ => false,
        }
    }

    pub(crate) fn contains_cell(&self, ty: Ty, target: LocId, locations: &[Ty]) -> bool {
        self.any_with_cells(
            ty,
            locations,
            &mut Vec::new(),
            &mut BitSet::default(),
            &|loc, _, _| (loc == target).then_some(true),
            &|_| false,
        )
    }

    pub(crate) fn intern_hir(&mut self, ctx: &Context, ty: &HirType, explicit: bool) -> Ty {
        match ty {
            HirType::Any => Ty::OPEN,
            HirType::Unknown => {
                if explicit {
                    Ty::DYNAMIC
                } else {
                    Ty::OPEN
                }
            }
            HirType::Never => Ty::NEVER,
            HirType::Void => Ty::VOID,
            HirType::Null => Ty::NULL,
            HirType::Boolean => Ty::BOOLEAN,
            HirType::Number => Ty::NUMBER,
            HirType::Int32 => Ty::INT32,
            HirType::BigInt => Ty::BIGINT,
            HirType::String => Ty::STRING,
            HirType::Symbol => Ty::SYMBOL,
            HirType::TypeVar(_) => self.type_var(shapes::TypeId::intern(ctx, ty)),
            HirType::StringLiteral(_) | HirType::Named(_) | HirType::Function(_) => {
                self.leaf(shapes::TypeId::intern(ctx, ty))
            }
            HirType::Array(i) => {
                let t = self.intern_hir(ctx, i, explicit);
                self.array(t)
            }
            HirType::Promise(i) => {
                let t = self.intern_hir(ctx, i, explicit);
                self.promise(t)
            }
            HirType::Tuple(v) => {
                let items: Vec<Ty> = v
                    .iter()
                    .map(|i| self.intern_hir(ctx, i, explicit))
                    .collect();
                self.tuple(&items)
            }
            HirType::Union(v) => {
                let items: Vec<Ty> = v
                    .iter()
                    .map(|m| self.intern_hir(ctx, m, explicit))
                    .collect();
                self.union(&items)
            }
            HirType::Generic { base, type_args } => {
                let base_id = shapes::TypeId::from_owned(ctx, HirType::Named(base.clone()));
                let args: Vec<Ty> = type_args
                    .iter()
                    .map(|a| self.intern_hir(ctx, a, explicit))
                    .collect();
                self.generic(base_id, &args)
            }
            HirType::Object(obj) => {
                let mut keys: Vec<&String> = match &obj.property_order {
                    Some(order) => order.iter().collect(),
                    None => {
                        let mut k: Vec<&String> = obj.properties.keys().collect();
                        k.sort_unstable();
                        k
                    }
                };
                keys.retain(|k| obj.properties.contains_key(*k));
                let props: Vec<ObjectProp> = keys
                    .into_iter()
                    .filter_map(|name| {
                        let info = obj.properties.get(name)?;
                        Some(ObjectProp {
                            name: self.intern_name(name),
                            ty: self.intern_hir(ctx, &info.ty, explicit),
                            optional: info.optional,
                            readonly: info.readonly,
                        })
                    })
                    .collect();
                let index = obj
                    .index_signature
                    .as_deref()
                    .map(|i| self.intern_hir(ctx, i, explicit));
                self.object(&props, index)
            }
        }
    }

    pub(crate) fn is_zero_sized_func(&self, ty: Ty, capture_free: &BitSet) -> bool {
        matches!(
            self.kind(ty),
            TyKind::Func(id) if id.capture_owner.is_none() && capture_free.contains(id.template.index())
        )
    }

    pub(crate) fn is_env_func(&self, ty: Ty, escaping_env: &BitSet) -> bool {
        matches!(
            self.kind(ty),
            TyKind::Func(id) if escaping_env.contains(id.template.index())
        )
    }

    pub(crate) fn is_tagged_func_union(
        &self,
        ty: Ty,
        capture_free: &BitSet,
        escaping_env: &BitSet,
    ) -> bool {
        let TyKind::Union(span) = self.kind(ty) else {
            return false;
        };
        let members = self.ty_slice(span);
        let mut func_count = 0usize;
        for &m in members {
            if matches!(m, Ty::NULL | Ty::VOID) {
                continue;
            }
            if self.is_zero_sized_func(m, capture_free) || self.is_env_func(m, escaping_env) {
                func_count += 1;
            } else {
                return false;
            }
        }
        func_count >= 1
    }

    pub(crate) fn func_union_members(&self, ty: Ty) -> Option<Vec<FuncIdentity>> {
        let TyKind::Union(span) = self.kind(ty) else {
            return None;
        };
        let members = self.ty_slice(span);
        let mut out = Vec::with_capacity(members.len());
        for &m in members {
            if matches!(m, Ty::NULL | Ty::VOID) {
                continue;
            }
            match self.kind(m) {
                TyKind::Func(id) => out.push(id),
                _ => return None,
            }
        }
        (out.len() >= 2).then_some(out)
    }

    pub(crate) fn func_members<'t>(
        &'t self,
        ty: &'t Ty,
    ) -> impl Iterator<Item = FuncIdentity> + Clone + 't {
        let members = match self.kind(*ty) {
            TyKind::Union(span) => self.ty_slice(span),
            _ => std::slice::from_ref(ty),
        };
        members
            .iter()
            .filter_map(|&member| match self.kind(member) {
                TyKind::Func(identity) => Some(identity),
                _ => None,
            })
    }

    pub(crate) fn has_func(&self, ty: Ty) -> bool {
        self.func_members(&ty).next().is_some()
    }

    pub(crate) fn contains_func_template(
        &self,
        ty: Ty,
        target: TemplateId,
        exclude_owner: Option<CaptureOwner>,
    ) -> bool {
        fn walk(
            table: &TypeTable,
            ty: Ty,
            target: TemplateId,
            exclude_owner: Option<CaptureOwner>,
            visited: &mut BitSet,
        ) -> bool {
            if !visited.insert(ty.index()) {
                return false;
            }
            if let TyKind::Func(ident) = table.kind(ty) {
                return ident.template == target && Some(ident.capture_owner) != exclude_owner;
            }
            table.any_child(ty, &mut |c| walk(table, c, target, exclude_owner, visited))
        }
        walk(self, ty, target, exclude_owner, &mut BitSet::default())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn materialize_hir_with(
        &self,
        ctx: &Context,
        ty: Ty,
        locations: &[Ty],
        dict_reads: &super::bitset::BitSet,
        capture_free: &super::bitset::BitSet,
        escaping_env: &super::bitset::BitSet,
        class_name: &impl Fn(ClassInstId) -> Option<String>,
    ) -> HirType {
        Materializer {
            table: self,
            ctx,
            locations,
            dict_reads,
            capture_free,
            escaping_env,
            class_name,
            visiting: Vec::new(),
        }
        .run(ty)
    }

    pub(crate) fn materialize_hir(
        &self,
        ctx: &Context,
        ty: Ty,
        class_name: &impl Fn(ClassInstId) -> Option<String>,
    ) -> HirType {
        let empty = super::bitset::BitSet::default();
        self.materialize_hir_with(ctx, ty, &[], &empty, &empty, &empty, class_name)
    }

    pub(crate) fn is_valid_dict_value(&self, elem: Ty, dict_read: bool) -> bool {
        let primitive = matches!(
            elem,
            Ty::NUMBER | Ty::INT32 | Ty::STRING | Ty::BOOLEAN | Ty::BIGINT
        );
        elem != Ty::BOTTOM
            && !self.is_open(elem)
            && elem != Ty::DYNAMIC
            && !matches!(self.kind(elem), TyKind::Union(_))
            && (!dict_read || primitive)
    }

    pub(crate) fn substitute(
        &mut self,
        ctx: &Context,
        ty: Ty,
        bindings: &[(shapes::TypeId, Ty)],
    ) -> Ty {
        if bindings.is_empty() {
            return ty;
        }
        match self.kind(ty) {
            TyKind::TypeVar(id) | TyKind::Leaf(id) => {
                if let HirType::TypeVar(name) | HirType::Named(name) = id.hir(ctx)
                    && let Some(&(_, bound)) = bindings.iter().find(|(p, _)| matches!(p.hir(ctx), HirType::Named(n) | HirType::TypeVar(n) if n == name))
                {
                    return bound;
                }
                ty
            }
            TyKind::Array(i) => {
                let n = self.substitute(ctx, i, bindings);
                if n == i { ty } else { self.array(n) }
            }
            TyKind::Promise(i) => {
                let n = self.substitute(ctx, i, bindings);
                if n == i { ty } else { self.promise(n) }
            }
            kind @ (TyKind::Tuple(s) | TyKind::Union(s) | TyKind::Generic { args: s, .. }) => {
                let mut changed = false;
                let out: Vec<Ty> = self
                    .ty_slice(s)
                    .to_vec()
                    .into_iter()
                    .map(|i| {
                        let n = self.substitute(ctx, i, bindings);
                        changed |= n != i;
                        n
                    })
                    .collect();
                if !changed {
                    return ty;
                }
                match kind {
                    TyKind::Tuple(_) => self.tuple(&out),
                    TyKind::Union(_) => self.union(&out),
                    TyKind::Generic { base, .. } => self.generic(base, &out),
                    _ => ty,
                }
            }
            TyKind::Object { props, index } => {
                let mut items = self.object_slice(props).to_vec();
                let mut changed = false;
                for p in &mut items {
                    let n = self.substitute(ctx, p.ty, bindings);
                    changed |= n != p.ty;
                    p.ty = n;
                }
                let next_idx = index.map(|i| {
                    let n = self.substitute(ctx, i, bindings);
                    changed |= n != i;
                    n
                });
                if changed {
                    self.object(&items, next_idx)
                } else {
                    ty
                }
            }
            _ => ty,
        }
    }

    pub(crate) fn unify(
        &self,
        ctx: &Context,
        pattern: Ty,
        actual: Ty,
        type_params: &[shapes::TypeId],
        bindings: &mut Vec<(shapes::TypeId, Ty)>,
    ) {
        if matches!(actual, Ty::BOTTOM | Ty::OPEN | Ty::NEVER) {
            return;
        }
        match (self.kind(pattern), self.kind(actual)) {
            (TyKind::TypeVar(id) | TyKind::Leaf(id), _) => {
                if let HirType::TypeVar(name) | HirType::Named(name) = id.hir(ctx)
                    && let Some(&tp) = type_params.iter().find(|tp| matches!(tp.hir(ctx), HirType::Named(p) | HirType::TypeVar(p) if p == name))
                    && !bindings.iter().any(|(k, _)| *k == tp)
                {
                    bindings.push((tp, actual));
                }
            }
            (TyKind::Array(p), TyKind::Array(a)) | (TyKind::Promise(p), TyKind::Promise(a)) => self.unify(ctx, p, a, type_params, bindings),
            (TyKind::Tuple(p), TyKind::Tuple(a)) => for (&pt, &at) in self.ty_slice(p).iter().zip(self.ty_slice(a)) { self.unify(ctx, pt, at, type_params, bindings); },
            (TyKind::Generic { base: pb, args: pa }, TyKind::Generic { base: ab, args: aa }) if pb == ab => {
                for (&pt, &at) in self.ty_slice(pa).iter().zip(self.ty_slice(aa)) { self.unify(ctx, pt, at, type_params, bindings); }
            }
            (TyKind::Object { props: pp, index: pi }, TyKind::Object { props: ap, index: ai }) => {
                let a_slice = self.object_slice(ap);
                for prop in self.object_slice(pp) {
                    if let Some(ap) = a_slice.iter().find(|p| p.name == prop.name) { self.unify(ctx, prop.ty, ap.ty, type_params, bindings); }
                }
                if let (Some(pt), Some(at)) = (pi, ai) { self.unify(ctx, pt, at, type_params, bindings); }
            }
            _ => {}
        }
    }

    pub(crate) fn accepts(&self, ctx: &Context, expected: Ty, actual: Ty) -> bool {
        if expected == actual || actual == Ty::NEVER || expected == Ty::DYNAMIC {
            return true;
        }
        if let TyKind::Union(s) = self.kind(actual) {
            return self
                .ty_slice(s)
                .iter()
                .all(|&m| self.accepts(ctx, expected, m));
        }
        if let TyKind::Union(s) = self.kind(expected) {
            return self
                .ty_slice(s)
                .iter()
                .any(|&m| self.accepts(ctx, m, actual));
        }
        match (self.kind(expected), self.kind(actual)) {
            (TyKind::Number, TyKind::Int32) => true,
            (TyKind::String, TyKind::Leaf(id)) => matches!(id.hir(ctx), HirType::StringLiteral(_)),
            _ => false,
        }
    }

    fn has_nested_unresolved_walk(&self, ty: Ty, nested: bool, seen: &mut [BitSet; 2]) -> bool {
        if !seen[usize::from(nested)].insert(ty.index()) {
            return false;
        }
        match self.kind(ty) {
            TyKind::Bottom | TyKind::Open | TyKind::Dynamic | TyKind::TypeVar(_) => true,
            TyKind::Tuple(s) => {
                let items = self.ty_slice(s);
                (nested && items.windows(2).any(|w| matches!(w, [a, b] if a != b)))
                    || items
                        .iter()
                        .any(|&t| self.has_nested_unresolved_walk(t, true, seen))
            }
            TyKind::Union(s) => {
                let ms = self.ty_slice(s);
                let all_funcs = ms
                    .iter()
                    .filter(|&&m| m != Ty::NULL && m != Ty::VOID)
                    .all(|&m| matches!(self.kind(m), TyKind::Func(_)));
                (nested
                    && !all_funcs
                    && ms
                        .iter()
                        .filter(|&&m| m != Ty::NULL && m != Ty::VOID)
                        .count()
                        > 1)
                    || ms
                        .iter()
                        .any(|&t| self.has_nested_unresolved_walk(t, nested, seen))
            }
            _ => self.any_child(ty, &mut |c| self.has_nested_unresolved_walk(c, true, seen)),
        }
    }

    pub(crate) fn has_nested_unresolved(&self, ty: Ty, nested: bool) -> bool {
        self.has_nested_unresolved_walk(ty, nested, &mut [BitSet::default(), BitSet::default()])
    }

    fn contains_ty_walk(&self, ty: Ty, needle: Ty, visited: &mut BitSet) -> bool {
        if ty == needle {
            return true;
        }
        if !visited.insert(ty.index()) {
            return false;
        }
        self.any_child(ty, &mut |c| self.contains_ty_walk(c, needle, visited))
    }

    pub(crate) fn contains_ty(&self, haystack: Ty, needle: Ty) -> bool {
        self.contains_ty_walk(haystack, needle, &mut BitSet::default())
    }

    fn contains_nested_ty(&self, haystack: Ty, needle: Ty) -> bool {
        let mut visited = BitSet::default();
        match self.kind(haystack) {
            TyKind::Union(s) => self.ty_slice(s).iter().any(|&m| {
                Self::is_composite(self.kind(m))
                    && self.any_child(m, &mut |c| self.contains_ty_walk(c, needle, &mut visited))
            }),
            k if Self::is_composite(k) => self.any_child(haystack, &mut |c| {
                self.contains_ty_walk(c, needle, &mut visited)
            }),
            _ => false,
        }
    }

    /// True when every branch of `ty` encodes cleanly as JSON (used to flag unannotated deferred
    /// union bindings as eligible for JSON storage if lowering cannot represent the union natively).
    pub(crate) fn is_json_encodable(
        &self,
        ctx: &Context,
        ty: Ty,
        locations: &[Ty],
        dict_reads: &BitSet,
    ) -> bool {
        let TyKind::Union(span) = self.kind(ty) else {
            return false;
        };
        let mut defined_count = 0usize;
        for &m in self.ty_slice(span) {
            if m != Ty::NULL && m != Ty::VOID {
                defined_count += 1;
            }
        }
        if defined_count < 2 {
            return false;
        }
        !self.any_with_cells(
            ty,
            locations,
            &mut Vec::new(),
            &mut BitSet::default(),
            &|loc, kind, elem| match kind {
                CellKind::Dict
                    if !self.is_valid_dict_value(elem, dict_reads.contains(loc.index())) =>
                {
                    Some(true)
                }
                CellKind::Promise => Some(true),
                _ => None,
            },
            &|k| {
                !matches!(
                    k,
                    TyKind::Null
                        | TyKind::Void
                        | TyKind::Boolean
                        | TyKind::Number
                        | TyKind::Int32
                        | TyKind::String
                        | TyKind::Dynamic
                        | TyKind::Array(_)
                        | TyKind::Cell { .. }
                        | TyKind::Tuple(_)
                        | TyKind::Union(_)
                        | TyKind::Object { .. }
                ) && !matches!(k, TyKind::Leaf(id) if matches!(id.hir(ctx), HirType::StringLiteral(_)))
            },
        )
    }
}

struct Materializer<'a, F> {
    table: &'a TypeTable,
    ctx: &'a Context,
    locations: &'a [Ty],
    dict_reads: &'a super::bitset::BitSet,
    capture_free: &'a super::bitset::BitSet,
    escaping_env: &'a super::bitset::BitSet,
    class_name: &'a F,
    visiting: Vec<LocId>,
}

impl<F: Fn(ClassInstId) -> Option<String>> Materializer<'_, F> {
    fn empty_object(index_signature: Option<Box<HirType>>) -> HirType {
        HirType::Object(ObjectType {
            name: None,
            properties: HashMap::new(),
            property_order: None,
            index_signature,
        })
    }
    fn run_cell(&mut self, loc: LocId, ty: Ty) -> HirType {
        self.visiting.push(loc);
        let hir = self.run(ty);
        self.visiting.pop();
        hir
    }
    fn run_slot(&mut self, ty: Ty) -> HirType {
        if self.table.is_zero_sized_func(ty, self.capture_free) {
            HirType::Number
        } else {
            self.run(ty)
        }
    }
    fn func_env_hir(ident: FuncIdentity) -> HirType {
        let mut properties = HashMap::with_capacity(2);
        properties.insert(
            "tag".to_owned(),
            PropertyInfo {
                ty: HirType::StringLiteral(func_tag(ident)),
                optional: false,
                readonly: false,
            },
        );
        properties.insert(
            "val".to_owned(),
            PropertyInfo {
                ty: HirType::Named(env_class_name(ident)),
                optional: false,
                readonly: false,
            },
        );
        HirType::Object(ObjectType {
            name: None,
            properties,
            property_order: Some(vec!["tag".to_owned(), "val".to_owned()]),
            index_signature: None,
        })
    }
    fn run(&mut self, ty: Ty) -> HirType {
        match self.table.kind(ty) {
            TyKind::Bottom | TyKind::Open => HirType::Any,
            TyKind::Dynamic => HirType::Unknown,
            TyKind::Never => HirType::Never,
            TyKind::Void => HirType::Void,
            TyKind::Null => HirType::Null,
            TyKind::Boolean => HirType::Boolean,
            TyKind::Number => HirType::Number,
            TyKind::Int32 => HirType::Int32,
            TyKind::BigInt => HirType::BigInt,
            TyKind::String => HirType::String,
            TyKind::Symbol => HirType::Symbol,
            TyKind::Leaf(id) | TyKind::TypeVar(id) => id.hir(self.ctx).clone(),
            TyKind::Array(i) => HirType::Array(Box::new(self.run_slot(i))),
            TyKind::Promise(i) => {
                let inner = if i == Ty::BOTTOM {
                    HirType::Void
                } else {
                    self.run(i)
                };
                HirType::Promise(Box::new(inner))
            }
            TyKind::Cell { loc, kind } => {
                if self.visiting.contains(&loc) {
                    return match kind {
                        CellKind::Array | CellKind::LocalArray => {
                            HirType::Array(Box::new(HirType::Any))
                        }
                        CellKind::Promise => HirType::Promise(Box::new(HirType::Any)),
                        CellKind::Dict => Self::empty_object(None),
                    };
                }
                let elem = self.locations.get(loc.index()).copied().unwrap_or(Ty::OPEN);
                match kind {
                    CellKind::Array | CellKind::LocalArray => {
                        let inner = if self.table.is_zero_sized_func(elem, self.capture_free) {
                            HirType::Number
                        } else {
                            self.run_cell(loc, elem)
                        };
                        HirType::Array(Box::new(inner))
                    }
                    CellKind::Promise => {
                        let inner = if elem == Ty::BOTTOM {
                            HirType::Void
                        } else {
                            self.run_cell(loc, elem)
                        };
                        HirType::Promise(Box::new(inner))
                    }
                    CellKind::Dict => {
                        let idx = if self
                            .table
                            .is_valid_dict_value(elem, self.dict_reads.contains(loc.index()))
                        {
                            Some(Box::new(self.run_cell(loc, elem)))
                        } else {
                            None
                        };
                        Self::empty_object(idx)
                    }
                }
            }
            TyKind::Tuple(s) => HirType::Tuple(
                self.table
                    .ty_slice(s)
                    .iter()
                    .map(|&t| self.run_slot(t))
                    .collect(),
            ),
            TyKind::Union(s) => {
                let slice = self.table.ty_slice(s);
                let has_func = slice
                    .iter()
                    .any(|&m| matches!(self.table.kind(m), TyKind::Func(_)));
                let mut members = Vec::with_capacity(s.len as usize);
                for &t in slice {
                    let h = if has_func && let TyKind::Func(ident) = self.table.kind(t) {
                        if self.escaping_env.contains(ident.template.index()) {
                            Self::func_env_hir(ident)
                        } else {
                            func_tag_hir(ident)
                        }
                    } else {
                        self.run(t)
                    };
                    if !members.contains(&h) {
                        members.push(h);
                    }
                }
                match members.len() {
                    0 => HirType::Never,
                    1 => members.pop().unwrap_or(HirType::Never),
                    _ => HirType::Union(members),
                }
            }
            TyKind::Generic { base, args } => HirType::Generic {
                base: match base.hir(self.ctx) {
                    HirType::Named(n) => n.clone(),
                    _ => String::new(),
                },
                type_args: self
                    .table
                    .ty_slice(args)
                    .iter()
                    .map(|&t| self.run(t))
                    .collect(),
            },
            TyKind::Object { props, index } => {
                let mut properties = HashMap::with_capacity(props.len as usize);
                let mut order = Vec::with_capacity(props.len as usize);
                for prop in self.table.object_slice(props) {
                    if self.table.is_zero_sized_func(prop.ty, self.capture_free) {
                        continue;
                    }
                    let name = self.table.name_str(prop.name).to_owned();
                    order.push(name.clone());
                    properties.insert(
                        name,
                        PropertyInfo {
                            ty: self.run(prop.ty),
                            optional: prop.optional,
                            readonly: prop.readonly,
                        },
                    );
                }
                let idx = index.map(|t| Box::new(self.run(t)));
                HirType::Object(ObjectType {
                    name: None,
                    properties,
                    property_order: (!order.is_empty()).then_some(order),
                    index_signature: idx,
                })
            }
            TyKind::ClassInst(inst) => HirType::Named((self.class_name)(inst).unwrap_or_default()),
            TyKind::Func(ident) if self.escaping_env.contains(ident.template.index()) => {
                Self::func_env_hir(ident)
            }
            TyKind::Func(_) => HirType::Function(perry_hir::types::FunctionType {
                params: Vec::new(),
                return_type: Box::new(HirType::Void),
                is_async: false,
                is_generator: false,
            }),
        }
    }
}

pub(crate) fn func_tag(ident: FuncIdentity) -> String {
    match ident.capture_owner {
        CaptureOwner::None => format!("fn_{}", ident.template.index()),
        CaptureOwner::Instance(owner) => format!("fn_{}_{}", ident.template.index(), owner.index()),
        CaptureOwner::Template(owner) => {
            format!("fn_{}_t{}", ident.template.index(), owner.index())
        }
    }
}

pub(crate) fn env_class_name(ident: FuncIdentity) -> String {
    format!("__Env_{}", func_tag(ident))
}

pub(crate) fn box_hir(inner: HirType) -> HirType {
    let mut properties = HashMap::with_capacity(1);
    properties.insert(
        "v".to_owned(),
        PropertyInfo {
            ty: inner,
            optional: false,
            readonly: false,
        },
    );
    HirType::Object(ObjectType {
        name: None,
        properties,
        property_order: Some(vec!["v".to_owned()]),
        index_signature: None,
    })
}

fn func_tag_hir(ident: FuncIdentity) -> HirType {
    let mut properties = HashMap::with_capacity(1);
    properties.insert(
        "tag".to_owned(),
        PropertyInfo {
            ty: HirType::StringLiteral(func_tag(ident)),
            optional: false,
            readonly: false,
        },
    );
    HirType::Object(ObjectType {
        name: None,
        properties,
        property_order: Some(vec!["tag".to_owned()]),
        index_signature: None,
    })
}
