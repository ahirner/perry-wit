//! Interned solver types, canonical unions, and monotone joins.
mod idx;
mod wit;
use super::bitset::BitSet;
use crate::{Absence, shapes};
pub(crate) use idx::*;
use perry_hir::types::Type as HirType;
use pliron::{builtin::types::IntegerType, context::Context, r#type::TypedHandle};
use std::collections::HashMap;
pub(crate) use wit::WitView;

const MAX_TYPE_DEPTH: u8 = 16;

#[derive(Clone, Copy, Default)]
struct TyMeta {
    contains_cell: bool,
    open: bool,
    depth: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TypeKey {
    Kind(TyKind),
    /// The ordinal distinguishes aliases in a specialization key. Only the element determines the layout.
    ArrayParameter {
        element: Ty,
        ordinal: usize,
    },
}

#[derive(Default)]
pub(crate) struct TypeTable {
    kinds: IdxVec<Ty, TyKind>,
    meta: IdxVec<Ty, TyMeta>,
    kind_index: HashMap<TypeKey, Ty>,
    members_index: HashMap<Box<[Ty]>, TySpan>,
    props_index: HashMap<Box<[ObjectProp]>, PropSpan>,
    ty_pool: Pool<TySpan, Ty>,
    object_props: Pool<PropSpan, ObjectProp>,
    names: IdxVec<NameId, String>,
    name_index: HashMap<String, NameId>,
    scratch: Vec<Ty>,
    pub(crate) applications: HashMap<shapes::generics::InstanceKey, shapes::generics::Application>,
    wit_shapes: HashMap<(*const wit_parser::Resolve, wit_parser::Type, WitView), Ty>,
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
            TyKind::Timer,
        ] {
            table.intern_kind(kind);
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
            TyKind::Tuple(s) | TyKind::Variant(s) | TyKind::Generic { args: s, .. } => {
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
        let contains_cell = matches!(kind, TyKind::Cell { .. })
            || self.any_child(ty, &mut |child| self.meta[child].contains_cell);
        let meta_ty = self.meta.push(TyMeta {
            open,
            depth,
            contains_cell,
        });
        debug_assert_eq!(ty, meta_ty);
        ty
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = Ty> + use<> {
        self.kinds.ids()
    }

    pub(crate) fn kind(&self, ty: Ty) -> TyKind {
        self.kinds[ty]
    }
    pub(crate) fn ty_slice(&self, span: TySpan) -> &[Ty] {
        &self.ty_pool[span]
    }
    pub(crate) fn tuple_slice(&self, ty: Ty) -> &[Ty] {
        match self.kind(ty) {
            TyKind::Tuple(span) => self.ty_slice(span),
            _ => &[],
        }
    }
    pub(crate) fn object_slice(&self, span: PropSpan) -> &[ObjectProp] {
        &self.object_props[span]
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

    fn intern_kind(&mut self, kind: TyKind) -> Ty {
        self.intern(TypeKey::Kind(kind))
    }

    fn intern(&mut self, key: TypeKey) -> Ty {
        if let Some(&ty) = self.kind_index.get(&key) {
            return ty;
        }
        let ty = self.push_kind(match key {
            TypeKey::Kind(kind) => kind,
            TypeKey::ArrayParameter { element, .. } => TyKind::Array(element),
        });
        self.kind_index.insert(key, ty);
        ty
    }
    pub(crate) fn named(&mut self, ctx: &Context, name: &str) -> Ty {
        self.intern_kind(TyKind::Named(shapes::TypeName::new(ctx, name.into())))
    }
    pub(crate) fn literal(&mut self, ctx: &Context, value: &str) -> Ty {
        self.intern_kind(TyKind::Literal(shapes::TypeName::new(ctx, value.into())))
    }
    pub(crate) fn array(&mut self, element: Ty) -> Ty {
        self.intern_kind(TyKind::Array(element))
    }
    pub(super) fn array_parameter(&mut self, element: Ty, ordinal: usize) -> Ty {
        self.intern(TypeKey::ArrayParameter { element, ordinal })
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

    pub(crate) fn application_key(
        &mut self,
        base: &str,
        args: &[Ty],
    ) -> shapes::generics::InstanceKey {
        (
            self.intern_name(base),
            self.ty_pool.intern(&mut self.members_index, args),
        )
    }

    pub(crate) fn tuple(&mut self, items: &[Ty]) -> Ty {
        let span = self.ty_pool.intern(&mut self.members_index, items);
        self.intern_kind(TyKind::Tuple(span))
    }
    pub(crate) fn generic(&mut self, base: shapes::TypeName, args: &[Ty]) -> Ty {
        let args = self.ty_pool.intern(&mut self.members_index, args);
        self.intern_kind(TyKind::Generic { base, args })
    }
    pub(crate) fn property(&mut self, name: &str, ty: Ty, optional: bool) -> ObjectProp {
        ObjectProp {
            name: self.intern_name(name),
            ty,
            optional,
            readonly: false,
        }
    }

    pub(crate) fn closed_record(&self, ty: Ty) -> Option<PropSpan> {
        match self.kind(ty) {
            TyKind::Object { props, index: None } => Some(props),
            _ => None,
        }
    }

    pub(crate) fn intern_properties(&mut self, props: &[ObjectProp]) -> PropSpan {
        self.object_props.intern(&mut self.props_index, props)
    }

    pub(crate) fn object_type(&mut self, props: PropSpan, index: Option<Ty>) -> Ty {
        self.intern_kind(TyKind::Object { props, index })
    }

    pub(crate) fn object(&mut self, props: &[ObjectProp], index: Option<Ty>) -> Ty {
        let span = self.intern_properties(props);
        self.object_type(span, index)
    }

    fn finish_union(&mut self, flat: &mut Vec<Ty>) -> Ty {
        if flat.contains(&Ty::OPEN) {
            Ty::OPEN
        } else if flat.contains(&Ty::DYNAMIC) {
            Ty::DYNAMIC
        } else {
            flat.sort_unstable();
            flat.dedup();
            self.intern_sorted_union(flat)
        }
    }

    pub(crate) fn union(&mut self, members: &[Ty]) -> Ty {
        let mut flat = std::mem::take(&mut self.scratch);
        flat.clear();
        for &m in members {
            self.push_union_member(&mut flat, m);
        }
        let res = self.finish_union(&mut flat);
        self.scratch = flat;
        res
    }

    pub(crate) fn union_span(&mut self, span: TySpan) -> Ty {
        let mut flat = std::mem::take(&mut self.scratch);
        flat.clear();
        for &ty in self.ty_slice(span) {
            self.push_union_member(&mut flat, ty);
        }
        let res = self.finish_union(&mut flat);
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
                let span = self.ty_pool.intern(&mut self.members_index, slice);
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

    pub(crate) fn integer_domain(
        &self,
        ctx: &Context,
        decls: &shapes::Declarations<'_>,
        ty: Ty,
    ) -> Option<TypedHandle<IntegerType>> {
        match self.kind(ty) {
            TyKind::Integer(domain) => Some(domain),
            TyKind::Named(name) => {
                let (width, signedness) = decls.integer_spec(ctx, name.get(ctx))?;
                Some(IntegerType::get(ctx, width, signedness))
            }
            _ => None,
        }
    }

    fn normalize_atom(&self, ctx: &Context, decls: &shapes::Declarations<'_>, ty: Ty) -> Ty {
        match self.kind(ty) {
            TyKind::Int32 => Ty::NUMBER,
            TyKind::Literal(_) => Ty::STRING,
            _ if self
                .integer_domain(ctx, decls, ty)
                .is_some_and(|domain| domain.deref(ctx).width() < 64) =>
            {
                Ty::NUMBER
            }
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

    pub(super) fn any_child(&self, ty: Ty, f: &mut impl FnMut(Ty) -> bool) -> bool {
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
        resolve: &mut impl FnMut(LocId) -> (LocId, Ty),
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
            let (loc, elem) = resolve(loc);
            if let Some(res) = on_cell(loc, ck, elem) {
                return res;
            }
            if visiting.contains(&loc) {
                return on_leaf(TyKind::Open);
            }
            visiting.push(loc);
            let hit = self.any_with_cells(elem, resolve, visiting, seen, on_cell, on_leaf);
            visiting.pop();
            return hit;
        }
        if visiting.is_empty() && !seen.insert(ty.index()) {
            return false;
        }
        self.any_child(ty, &mut |c| {
            self.any_with_cells(c, resolve, visiting, seen, on_cell, on_leaf)
        })
    }

    pub(crate) fn is_open_with(
        &self,
        ty: Ty,
        mut resolve: impl FnMut(LocId) -> (LocId, Ty),
        dict_reads: &BitSet,
    ) -> bool {
        self.any_with_cells(
            ty,
            &mut resolve,
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

    pub(super) fn promise_argument(&mut self, ty: Ty, mut read: impl FnMut(LocId) -> Ty) -> Ty {
        if let TyKind::Cell {
            loc,
            kind: CellKind::Promise,
        } = self.kind(ty)
        {
            let inner = read(loc);
            self.promise(if inner == Ty::BOTTOM { Ty::VOID } else { inner })
        } else {
            ty
        }
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

    pub(crate) fn contains_cell(
        &self,
        ty: Ty,
        target: LocId,
        mut resolve: impl FnMut(LocId) -> (LocId, Ty),
    ) -> bool {
        self.any_with_cells(
            ty,
            &mut resolve,
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
            HirType::TypeVar(name) => {
                self.intern_kind(TyKind::TypeVar(shapes::TypeName::new(ctx, name.clone())))
            }
            HirType::StringLiteral(value) => self.literal(ctx, value),
            HirType::Named(name) => self.named(ctx, name),
            HirType::Function(signature) => {
                let params: Vec<_> = signature
                    .params
                    .iter()
                    .map(|(name, ty, optional)| {
                        let ty = self.intern_hir(ctx, ty, explicit);
                        self.property(name, ty, *optional)
                    })
                    .collect();
                let params = self.intern_properties(&params);
                let result = self.intern_hir(ctx, &signature.return_type, explicit);
                self.intern_kind(TyKind::Signature {
                    params,
                    result,
                    is_async: signature.is_async,
                    is_generator: signature.is_generator,
                })
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
                let base_id = shapes::TypeName::new(ctx, base.clone());
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

    pub(super) fn erased_function(&self, ty: Ty, escaping_env: &BitSet) -> Option<FuncIdentity> {
        match self.kind(ty) {
            TyKind::Func(id) if !escaping_env.contains(id.template.index()) => Some(id),
            _ => None,
        }
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

    pub(crate) fn func_dispatch_members(
        &self,
        ty: Ty,
        allow_nullish_single: bool,
    ) -> Option<Vec<FuncIdentity>> {
        let TyKind::Union(span) = self.kind(ty) else {
            return None;
        };
        let members = self.ty_slice(span);
        let mut out = Vec::with_capacity(members.len());
        let mut has_nullish = false;
        for &m in members {
            if matches!(m, Ty::NULL | Ty::VOID) {
                has_nullish = true;
            } else if let TyKind::Func(id) = self.kind(m) {
                out.push(id);
            } else {
                return None;
            }
        }
        (out.len() >= 2 || (allow_nullish_single && has_nullish && out.len() == 1)).then_some(out)
    }

    pub(super) fn alternatives<'t>(&'t self, ty: &'t Ty) -> &'t [Ty] {
        match self.kind(*ty) {
            TyKind::Union(span) => self.ty_slice(span),
            _ => std::slice::from_ref(ty),
        }
    }

    pub(crate) fn func_members<'t>(
        &'t self,
        ty: &'t Ty,
    ) -> impl Iterator<Item = FuncIdentity> + Clone + 't {
        self.alternatives(ty)
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

    pub(crate) fn substitute(&mut self, ty: Ty, bindings: &[(shapes::TypeName, Ty)]) -> Ty {
        if bindings.is_empty() {
            return ty;
        }
        self.substitute_with(ty, |_, name| {
            bindings.iter().find(|(p, _)| *p == name).map(|&(_, ty)| ty)
        })
    }
    pub(super) fn substitute_with(
        &mut self,
        ty: Ty,
        binding: impl Fn(&Self, shapes::TypeName) -> Option<Ty>,
    ) -> Ty {
        self.rewrite(ty, &mut |types, ty| match types.kind(ty) {
            TyKind::TypeVar(name) | TyKind::Named(name) => binding(types, name),
            _ => None,
        })
    }

    pub(super) fn rewrite(
        &mut self,
        ty: Ty,
        replace: &mut impl FnMut(&mut TypeTable, Ty) -> Option<Ty>,
    ) -> Ty {
        if let Some(bound) = replace(self, ty) {
            return bound;
        }
        match self.kind(ty) {
            TyKind::Signature {
                params,
                result,
                is_async,
                is_generator,
            } => {
                let mut params = self.object_slice(params).to_vec();
                for p in &mut params {
                    p.ty = self.rewrite(p.ty, replace);
                }
                let params = self.intern_properties(&params);
                let result = self.rewrite(result, replace);
                self.intern_kind(TyKind::Signature {
                    params,
                    result,
                    is_async,
                    is_generator,
                })
            }
            TyKind::Array(i) => {
                let n = self.rewrite(i, replace);
                if n == i { ty } else { self.array(n) }
            }
            TyKind::Promise(i) => {
                let n = self.rewrite(i, replace);
                if n == i { ty } else { self.promise(n) }
            }
            kind @ (TyKind::Tuple(s) | TyKind::Union(s) | TyKind::Generic { args: s, .. }) => {
                let mut out = self.ty_slice(s).to_vec();
                for item in &mut out {
                    *item = self.rewrite(*item, replace);
                }
                if out == self.ty_slice(s) {
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
                for p in &mut items {
                    p.ty = self.rewrite(p.ty, replace);
                }
                let next_idx = index.map(|i| self.rewrite(i, replace));
                if items != self.object_slice(props) || next_idx != index {
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
        pattern: Ty,
        actual: Ty,
        type_params: &impl Fn(shapes::TypeName) -> Option<usize>,
        bindings: &mut [Ty],
    ) {
        if matches!(actual, Ty::BOTTOM | Ty::OPEN | Ty::NEVER) {
            return;
        }
        match (self.kind(pattern), self.kind(actual)) {
            (TyKind::TypeVar(name) | TyKind::Named(name), _)
                if let Some(slot) = type_params(name).and_then(|i| bindings.get_mut(i))
                    && *slot == Ty::OPEN =>
            {
                *slot = actual;
            }
            (TyKind::Array(p), TyKind::Array(a)) | (TyKind::Promise(p), TyKind::Promise(a)) => {
                self.unify(p, a, type_params, bindings)
            }
            (TyKind::Tuple(p), TyKind::Tuple(a)) => {
                for (&pt, &at) in self.ty_slice(p).iter().zip(self.ty_slice(a)) {
                    self.unify(pt, at, type_params, bindings);
                }
            }
            (TyKind::Generic { base: pb, args: pa }, TyKind::Generic { base: ab, args: aa })
                if pb == ab =>
            {
                for (&pt, &at) in self.ty_slice(pa).iter().zip(self.ty_slice(aa)) {
                    self.unify(pt, at, type_params, bindings);
                }
            }
            (
                TyKind::Object {
                    props: pp,
                    index: pi,
                },
                TyKind::Object {
                    props: ap,
                    index: ai,
                },
            ) => {
                let a_slice = self.object_slice(ap);
                for prop in self.object_slice(pp) {
                    if let Some(ap) = a_slice.iter().find(|p| p.name == prop.name) {
                        self.unify(prop.ty, ap.ty, type_params, bindings);
                    }
                }
                if let (Some(pt), Some(at)) = (pi, ai) {
                    self.unify(pt, at, type_params, bindings);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn accepts(&self, expected: Ty, actual: Ty) -> bool {
        if expected == actual || actual == Ty::NEVER || expected == Ty::DYNAMIC {
            return true;
        }
        if let TyKind::Union(s) = self.kind(actual) {
            return self.ty_slice(s).iter().all(|&m| self.accepts(expected, m));
        }
        if let TyKind::Union(s) = self.kind(expected) {
            return self.ty_slice(s).iter().any(|&m| self.accepts(m, actual));
        }
        matches!(
            (self.kind(expected), self.kind(actual)),
            (TyKind::Number, TyKind::Int32) | (TyKind::String, TyKind::Literal(_))
        )
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
