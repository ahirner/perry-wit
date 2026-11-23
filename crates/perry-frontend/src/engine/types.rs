//! Dense interned types over `shapes::TypeId`, canonical sorted unions, and monotone joins.
use crate::{Absence, shapes};
use perry_hir::types::{ObjectType, PropertyInfo, Type as HirType};
use pliron::context::Context;
use std::collections::HashMap;

macro_rules! id_type {
    ($($name:ident($vis:vis u32)),* $(,)?) => {
        $(#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)] pub(crate) struct $name($vis u32);)*
    };
}

id_type!(Ty(u32), TemplateId(pub(crate) u32), InstanceId(pub(crate) u32), ClassId(pub(crate) u32), ClassInstId(pub(crate) u32), LocId(pub(crate) u32), LocalIdx(pub(crate) u32), NameId(u32));

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
pub(crate) struct FuncIdentity {
    pub(crate) template: TemplateId,
    pub(crate) capture_owner: Option<InstanceId>,
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
}
impl CellKind {
    pub(crate) fn is_array(self) -> bool {
        matches!(self, Self::Array | Self::LocalArray)
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

#[derive(PartialEq, Eq, Hash)]
enum InternKey {
    Simple(TyKind),
    Tuple(Box<[Ty]>),
    Union(Box<[Ty]>),
    Generic(shapes::TypeId, Box<[Ty]>),
    Object(Box<[ObjectProp]>, Option<Ty>),
}

#[derive(Default)]
pub(crate) struct TypeTable {
    kinds: Vec<TyKind>,
    index: HashMap<InternKey, Ty>,
    ty_pool: Vec<Ty>,
    object_props: Vec<ObjectProp>,
    names: Vec<String>,
    name_index: HashMap<String, NameId>,
    scratch: Vec<Ty>,
}

impl TypeTable {
    pub(crate) fn new() -> Self {
        let mut table = Self {
            kinds: Vec::with_capacity(64),
            index: HashMap::with_capacity(64),
            name_index: HashMap::with_capacity(32),
            ..Self::default()
        };
        for (i, kind) in [
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
        ]
        .into_iter()
        .enumerate()
        {
            table.kinds.push(kind);
            table.index.insert(InternKey::Simple(kind), Ty(i as u32));
        }
        table
    }

    pub(crate) fn kind(&self, ty: Ty) -> TyKind {
        debug_assert!((ty.0 as usize) < self.kinds.len());
        self.kinds[ty.0 as usize]
    }
    pub(crate) fn ty_slice(&self, span: TySpan) -> &[Ty] {
        &self.ty_pool[span.range()]
    }
    pub(crate) fn object_slice(&self, span: TySpan) -> &[ObjectProp] {
        &self.object_props[span.range()]
    }
    pub(crate) fn name_str(&self, id: NameId) -> &str {
        debug_assert!((id.0 as usize) < self.names.len());
        &self.names[id.0 as usize]
    }

    pub(crate) fn intern_name(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.name_index.get(name) {
            return id;
        }
        let id = NameId(self.names.len() as u32);
        let owned = name.to_owned();
        self.names.push(owned.clone());
        self.name_index.insert(owned, id);
        id
    }

    fn push_span<T: Copy>(buf: &mut Vec<T>, items: &[T]) -> TySpan {
        let start = buf.len() as u32;
        buf.extend_from_slice(items);
        TySpan {
            start,
            len: items.len() as u32,
        }
    }

    fn intern_key(&mut self, key: InternKey, make_kind: impl FnOnce(&mut Self) -> TyKind) -> Ty {
        if let Some(&ty) = self.index.get(&key) {
            return ty;
        }
        let kind = make_kind(self);
        let ty = Ty(self.kinds.len() as u32);
        self.kinds.push(kind);
        self.index.insert(key, ty);
        ty
    }

    fn intern_simple(&mut self, kind: TyKind) -> Ty {
        self.intern_key(InternKey::Simple(kind), |_| kind)
    }
    pub(crate) fn leaf(&mut self, id: shapes::TypeId) -> Ty {
        self.intern_simple(TyKind::Leaf(id))
    }
    pub(crate) fn named(&mut self, ctx: &Context, name: &str) -> Ty {
        self.leaf(shapes::TypeId::from_owned(
            ctx,
            HirType::Named(name.to_owned()),
        ))
    }
    pub(crate) fn type_var(&mut self, id: shapes::TypeId) -> Ty {
        self.intern_simple(TyKind::TypeVar(id))
    }
    pub(crate) fn array(&mut self, element: Ty) -> Ty {
        self.intern_simple(TyKind::Array(element))
    }
    pub(crate) fn cell(&mut self, loc: LocId, kind: CellKind) -> Ty {
        self.intern_simple(TyKind::Cell { loc, kind })
    }
    pub(crate) fn array_cell(&mut self, loc: LocId) -> Ty {
        self.cell(loc, CellKind::Array)
    }
    pub(crate) fn promise(&mut self, inner: Ty) -> Ty {
        self.intern_simple(TyKind::Promise(inner))
    }
    pub(crate) fn func(&mut self, identity: FuncIdentity) -> Ty {
        self.intern_simple(TyKind::Func(identity))
    }
    pub(crate) fn class_inst(&mut self, inst: ClassInstId) -> Ty {
        self.intern_simple(TyKind::ClassInst(inst))
    }

    pub(crate) fn tuple(&mut self, items: &[Ty]) -> Ty {
        self.intern_key(InternKey::Tuple(items.into()), |t| {
            TyKind::Tuple(Self::push_span(&mut t.ty_pool, items))
        })
    }
    pub(crate) fn generic(&mut self, base: shapes::TypeId, args: &[Ty]) -> Ty {
        self.intern_key(InternKey::Generic(base, args.into()), |t| TyKind::Generic {
            base,
            args: Self::push_span(&mut t.ty_pool, args),
        })
    }
    pub(crate) fn object(&mut self, props: &[ObjectProp], index: Option<Ty>) -> Ty {
        self.intern_key(InternKey::Object(props.into(), index), |t| TyKind::Object {
            props: Self::push_span(&mut t.object_props, props),
            index,
        })
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
            slice => self.intern_key(InternKey::Union(slice.into()), |t| {
                TyKind::Union(Self::push_span(&mut t.ty_pool, slice))
            }),
        }
    }

    pub(crate) fn join(&mut self, a: Ty, b: Ty) -> Ty {
        if a == b || b == Ty::BOTTOM || b == Ty::NEVER {
            a
        } else if a == Ty::BOTTOM || a == Ty::NEVER {
            b
        } else if a == Ty::OPEN || b == Ty::OPEN {
            Ty::OPEN
        } else if a == Ty::DYNAMIC || b == Ty::DYNAMIC {
            Ty::DYNAMIC
        } else {
            self.union(&[a, b])
        }
    }

    pub(crate) fn join_bounded(&mut self, old: Ty, value: Ty) -> Ty {
        if value == Ty::BOTTOM || value == Ty::NEVER || old == value {
            return old;
        }
        let composite = matches!(
            self.kind(old),
            TyKind::Array(_)
                | TyKind::Object { .. }
                | TyKind::Tuple(_)
                | TyKind::Generic { .. }
                | TyKind::Promise(_)
        );
        self.join(
            old,
            if composite && self.contains_ty(value, old) {
                Ty::OPEN
            } else {
                value
            },
        )
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
        matches!(
            self.kind(ty),
            TyKind::Bottom | TyKind::Open | TyKind::TypeVar(_)
        ) || self.any_child(ty, &mut |c| self.is_open(c))
    }

    fn any_with_cells(
        &self,
        ty: Ty,
        locations: &[Ty],
        visiting: &mut Vec<LocId>,
        on_cell: &impl Fn(LocId, CellKind, Ty) -> Option<bool>,
        on_leaf: &impl Fn(TyKind) -> bool,
    ) -> bool {
        let kind = self.kind(ty);
        if on_leaf(kind) {
            return true;
        }
        if let TyKind::Cell { loc, kind: ck } = kind {
            let Some(&elem) = locations.get(loc.0 as usize) else {
                return on_cell(loc, ck, Ty::OPEN).unwrap_or(true);
            };
            if let Some(res) = on_cell(loc, ck, elem) {
                return res;
            }
            if visiting.contains(&loc) {
                return on_leaf(TyKind::Open);
            }
            visiting.push(loc);
            let hit = self.any_with_cells(elem, locations, visiting, on_cell, on_leaf);
            visiting.pop();
            return hit;
        }
        self.any_child(ty, &mut |c| {
            self.any_with_cells(c, locations, visiting, on_cell, on_leaf)
        })
    }

    pub(crate) fn is_open_with(
        &self,
        ty: Ty,
        locations: &[Ty],
        dict_reads: &super::bitset::BitSet,
    ) -> bool {
        self.any_with_cells(
            ty,
            locations,
            &mut Vec::new(),
            &|loc, ck, elem| {
                (ck == CellKind::Dict)
                    .then(|| !self.is_valid_dict_value(elem, dict_reads.contains(loc.0 as usize)))
            },
            &|k| matches!(k, TyKind::Bottom | TyKind::Open | TyKind::TypeVar(_)),
        )
    }

    pub(crate) fn contains_cell(&self, ty: Ty, target: LocId, locations: &[Ty]) -> bool {
        self.any_with_cells(
            ty,
            locations,
            &mut Vec::new(),
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

    pub(crate) fn materialize_hir_with(
        &self,
        ctx: &Context,
        ty: Ty,
        locations: &[Ty],
        dict_reads: &super::bitset::BitSet,
        class_name: &impl Fn(ClassInstId) -> Option<String>,
    ) -> HirType {
        Materializer {
            table: self,
            ctx,
            locations,
            dict_reads,
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
        self.materialize_hir_with(ctx, ty, &[], &super::bitset::BitSet::default(), class_name)
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

    pub(crate) fn has_nested_unresolved(&self, ty: Ty, nested: bool) -> bool {
        match self.kind(ty) {
            TyKind::Bottom | TyKind::Open | TyKind::Dynamic | TyKind::TypeVar(_) => true,
            TyKind::Tuple(s) => {
                let items = self.ty_slice(s);
                (nested && items.windows(2).any(|w| w[0] != w[1]))
                    || items.iter().any(|&t| self.has_nested_unresolved(t, true))
            }
            TyKind::Union(s) => {
                let ms = self.ty_slice(s);
                (nested
                    && ms
                        .iter()
                        .filter(|&&m| m != Ty::NULL && m != Ty::VOID)
                        .count()
                        > 1)
                    || ms.iter().any(|&t| self.has_nested_unresolved(t, nested))
            }
            _ => self.any_child(ty, &mut |c| self.has_nested_unresolved(c, true)),
        }
    }

    pub(crate) fn contains_ty(&self, haystack: Ty, needle: Ty) -> bool {
        haystack == needle || self.any_child(haystack, &mut |c| self.contains_ty(c, needle))
    }
}

struct Materializer<'a, F> {
    table: &'a TypeTable,
    ctx: &'a Context,
    locations: &'a [Ty],
    dict_reads: &'a super::bitset::BitSet,
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
            TyKind::Array(i) => HirType::Array(Box::new(self.run(i))),
            TyKind::Promise(i) => HirType::Promise(Box::new(self.run(i))),
            TyKind::Cell { loc, kind } => {
                if self.visiting.contains(&loc) {
                    return if kind.is_array() {
                        HirType::Array(Box::new(HirType::Any))
                    } else {
                        Self::empty_object(None)
                    };
                }
                let elem = self
                    .locations
                    .get(loc.0 as usize)
                    .copied()
                    .unwrap_or(Ty::OPEN);
                if kind.is_array() {
                    HirType::Array(Box::new(self.run_cell(loc, elem)))
                } else {
                    let idx = self
                        .table
                        .is_valid_dict_value(elem, self.dict_reads.contains(loc.0 as usize))
                        .then(|| Box::new(self.run_cell(loc, elem)));
                    Self::empty_object(idx)
                }
            }
            TyKind::Tuple(s) => HirType::Tuple(
                self.table
                    .ty_slice(s)
                    .iter()
                    .map(|&t| self.run(t))
                    .collect(),
            ),
            TyKind::Union(s) => {
                let mut members = Vec::with_capacity(s.len as usize);
                for &t in self.table.ty_slice(s) {
                    let h = self.run(t);
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
                HirType::Object(ObjectType {
                    name: None,
                    properties,
                    property_order: (!order.is_empty()).then_some(order),
                    index_signature: index.map(|t| Box::new(self.run(t))),
                })
            }
            TyKind::ClassInst(inst) => {
                let name = (self.class_name)(inst);
                debug_assert!(name.is_some());
                HirType::Named(name.unwrap_or_default())
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
