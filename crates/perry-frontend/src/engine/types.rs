//! Dense interned types over `shapes::TypeId`, canonical sorted unions, and monotone joins.
//!
//! `HirType` trees cross this boundary only on interning (`from_hir`) and emission (`to_hir`).
//! Everything inside the engine compares and joins `Copy` `Ty` indices with zero `HirType` clones.
use crate::{Absence, shapes};
use perry_hir::types::{ObjectType, PropertyInfo, Type as HirType};
use pliron::context::Context;
use std::collections::HashMap;

/// Dense index into [`TypeTable`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct Ty(u32);

/// Dense span `(start, len)` into one of [`TypeTable`]'s flat pools.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct TySpan {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

impl TySpan {
    pub(crate) fn range(self) -> std::ops::Range<usize> {
        let start = self.start as usize;
        start..start.saturating_add(self.len as usize)
    }
}

/// Dense template index (every function-like declaration and module init).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct TemplateId(pub(crate) u32);

/// Dense specialization instance index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct InstanceId(pub(crate) u32);

/// Dense user-class index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct ClassId(pub(crate) u32);

/// Dense instantiated-class index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct ClassInstId(pub(crate) u32);

/// Dense storage location index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct LocId(pub(crate) u32);

/// Dense local-variable index (remapped from `LocalId` at the boundary).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct LocalIdx(pub(crate) u32);

/// Dense interned property/symbol name index inside [`TypeTable`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct NameId(u32);

/// Static function identity: one template plus the enclosing instance that owns its local captures.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct FuncIdentity {
    pub(crate) template: TemplateId,
    pub(crate) capture_owner: Option<InstanceId>,
}

/// One property inside an interned structural object shape.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ObjectProp {
    pub(crate) name: NameId,
    pub(crate) ty: Ty,
    pub(crate) optional: bool,
    pub(crate) readonly: bool,
}

/// Kind of mutable empty-aggregate cell allocated for `[]`, `any[]`, or `{}`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CellKind {
    /// Unannotated `[]`: flows across calls, closures, and class fields.
    Array,
    /// Explicit `any[] = []`: inferred locally with conservative escape boundaries.
    LocalArray,
    /// Empty `{}`: dictionary inferred locally with conservative escape boundaries.
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
    /// Leaf concrete HIR type (`StringLiteral`, `Named`, `Function` annotation) interned in `Context`.
    Leaf(shapes::TypeId),
    /// Unbound type parameter name (`HirType::TypeVar`), interned as `shapes::TypeId`.
    TypeVar(shapes::TypeId),
    Array(Ty),
    /// Mutable empty-array or empty-object storage cell whose element/value type lives at `loc`.
    Cell {
        loc: LocId,
        kind: CellKind,
    },
    Promise(Ty),
    Tuple(TySpan),
    /// Canonical sorted, flattened, deduplicated union members in `union_members`.
    Union(TySpan),
    /// Generic instantiation `base<args>` where `base` is an interned `HirType::Named(base)`.
    Generic {
        base: shapes::TypeId,
        args: TySpan,
    },
    /// Structural object shape with properties sorted by `NameId` in `object_props`.
    Object {
        props: TySpan,
        index: Option<Ty>,
    },
    /// Static function identity (declaration or closure literal + capture owner).
    Func(FuncIdentity),
    /// Concrete instantiation of a user class.
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

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(PartialEq, Eq, Hash)]
enum InternKey {
    Simple(TyKind),
    Tuple(Box<[Ty]>),
    Union(Box<[Ty]>),
    Generic(shapes::TypeId, Box<[Ty]>),
    Object(Box<[ObjectProp]>, Option<Ty>),
}

pub(crate) struct TypeTable {
    kinds: Vec<TyKind>,
    index: HashMap<InternKey, Ty>,
    union_members: Vec<Ty>,
    tuple_items: Vec<Ty>,
    generic_args: Vec<Ty>,
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
            union_members: Vec::new(),
            tuple_items: Vec::new(),
            generic_args: Vec::new(),
            object_props: Vec::new(),
            names: Vec::new(),
            name_index: HashMap::with_capacity(32),
            scratch: Vec::new(),
        };
        let builtins = [
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
        ];
        for (i, kind) in builtins.into_iter().enumerate() {
            table.kinds.push(kind);
            table.index.insert(InternKey::Simple(kind), Ty(i as u32));
        }
        table
    }

    pub(crate) fn kind(&self, ty: Ty) -> TyKind {
        debug_assert!(ty.index() < self.kinds.len(), "invalid Ty({})", ty.0);
        self.kinds[ty.index()]
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

    pub(crate) fn name_str(&self, id: NameId) -> &str {
        debug_assert!(
            (id.0 as usize) < self.names.len(),
            "invalid NameId({})",
            id.0
        );
        &self.names[id.0 as usize]
    }

    pub(crate) fn union_slice(&self, span: TySpan) -> &[Ty] {
        &self.union_members[span.range()]
    }

    pub(crate) fn tuple_slice(&self, span: TySpan) -> &[Ty] {
        &self.tuple_items[span.range()]
    }

    pub(crate) fn generic_slice(&self, span: TySpan) -> &[Ty] {
        &self.generic_args[span.range()]
    }

    pub(crate) fn object_slice(&self, span: TySpan) -> &[ObjectProp] {
        &self.object_props[span.range()]
    }

    fn intern_simple(&mut self, kind: TyKind) -> Ty {
        let key = InternKey::Simple(kind);
        if let Some(&ty) = self.index.get(&key) {
            return ty;
        }
        let ty = Ty(self.kinds.len() as u32);
        self.kinds.push(kind);
        self.index.insert(key, ty);
        ty
    }

    pub(crate) fn leaf(&mut self, id: shapes::TypeId) -> Ty {
        self.intern_simple(TyKind::Leaf(id))
    }

    pub(crate) fn named(&mut self, ctx: &Context, name: &str) -> Ty {
        let id = shapes::TypeId::from_owned(ctx, HirType::Named(name.to_owned()));
        self.leaf(id)
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

    pub(crate) fn tuple(&mut self, items: &[Ty]) -> Ty {
        self.intern_key(InternKey::Tuple(items.into()), |t| {
            TyKind::Tuple(Self::push_span(&mut t.tuple_items, items))
        })
    }

    pub(crate) fn generic(&mut self, base: shapes::TypeId, args: &[Ty]) -> Ty {
        self.intern_key(InternKey::Generic(base, args.into()), |t| TyKind::Generic {
            base,
            args: Self::push_span(&mut t.generic_args, args),
        })
    }

    pub(crate) fn object(&mut self, props: &[ObjectProp], index: Option<Ty>) -> Ty {
        self.intern_key(InternKey::Object(props.into(), index), |t| TyKind::Object {
            props: Self::push_span(&mut t.object_props, props),
            index,
        })
    }

    /// Intern a canonical sorted, flattened, deduplicated union.
    pub(crate) fn union(&mut self, members: &[Ty]) -> Ty {
        let mut flat = std::mem::take(&mut self.scratch);
        flat.clear();
        for &member in members {
            self.push_union_member(&mut flat, member);
        }
        if flat.contains(&Ty::OPEN) {
            self.scratch = flat;
            return Ty::OPEN;
        }
        if flat.contains(&Ty::DYNAMIC) {
            self.scratch = flat;
            return Ty::DYNAMIC;
        }
        flat.sort_unstable();
        flat.dedup();
        let result = self.intern_sorted_union(&flat);
        self.scratch = flat;
        result
    }

    fn push_union_member(&self, out: &mut Vec<Ty>, ty: Ty) {
        if ty == Ty::BOTTOM || ty == Ty::NEVER {
            return;
        }
        if let TyKind::Union(span) = self.kind(ty) {
            out.extend_from_slice(self.union_slice(span));
        } else {
            out.push(ty);
        }
    }

    fn intern_sorted_union(&mut self, sorted: &[Ty]) -> Ty {
        match sorted {
            [] => Ty::NEVER,
            [single] => *single,
            slice => self.intern_key(InternKey::Union(slice.into()), |t| {
                TyKind::Union(Self::push_span(&mut t.union_members, slice))
            }),
        }
    }

    /// Monotone join of two types over the finite union lattice.
    /// Compares indices first for O(1) idempotence and identity checks.
    pub(crate) fn join(&mut self, a: Ty, b: Ty) -> Ty {
        if a == b {
            return a;
        }
        if a == Ty::BOTTOM || a == Ty::NEVER {
            return b;
        }
        if b == Ty::BOTTOM || b == Ty::NEVER {
            return a;
        }
        if a == Ty::OPEN || b == Ty::OPEN {
            return Ty::OPEN;
        }
        if a == Ty::DYNAMIC || b == Ty::DYNAMIC {
            return Ty::DYNAMIC;
        }
        self.union(&[a, b])
    }

    /// Monotone join with a composite-nesting height bound (widening cyclic self-nesting writes to `OPEN`).
    pub(crate) fn join_bounded(&mut self, old: Ty, value: Ty) -> Ty {
        if value == Ty::BOTTOM || value == Ty::NEVER || old == value {
            return old;
        }
        let old_is_composite = matches!(
            self.kind(old),
            TyKind::Array(_)
                | TyKind::Object { .. }
                | TyKind::Tuple(_)
                | TyKind::Generic { .. }
                | TyKind::Promise(_)
        );
        let widened = if old_is_composite && self.contains_ty(value, old) {
            Ty::OPEN
        } else {
            value
        };
        self.join(old, widened)
    }

    fn normalize_atom(&self, ctx: &Context, decls: &shapes::Declarations<'_>, ty: Ty) -> Ty {
        match self.kind(ty) {
            TyKind::Int32 => Ty::NUMBER,
            TyKind::Leaf(id) => match id.hir(ctx) {
                HirType::StringLiteral(_) => Ty::STRING,
                HirType::Named(name)
                    if matches!(decls.integer_carrier(name), Some(HirType::Number)) =>
                {
                    Ty::NUMBER
                }
                _ => ty,
            },
            _ => ty,
        }
    }

    /// Normalize a value written into storage (widening `Int32 -> Number`, `StringLiteral -> String`,
    /// and 32-bit WIT integer aliases to `Number`, matching #70's storage rules).
    pub(crate) fn normalize_for_storage(
        &mut self,
        ctx: &Context,
        decls: &shapes::Declarations<'_>,
        ty: Ty,
    ) -> Ty {
        if let TyKind::Union(span) = self.kind(ty) {
            let mut flat = std::mem::take(&mut self.scratch);
            flat.clear();
            for offset in 0..span.len as usize {
                let Some(&member) = self.union_members.get(span.start as usize + offset) else {
                    break;
                };
                let norm = self.normalize_atom(ctx, decls, member);
                if norm != Ty::BOTTOM && norm != Ty::NEVER {
                    flat.push(norm);
                }
            }
            flat.sort_unstable();
            flat.dedup();
            let result = self.intern_sorted_union(&flat);
            self.scratch = flat;
            result
        } else {
            self.normalize_atom(ctx, decls, ty)
        }
    }

    /// Separate `null` / `undefined` from a type, returning `(defined_part, absences)`.
    pub(crate) fn strip_nullish(&mut self, ty: Ty) -> (Ty, Option<Absence>) {
        match self.kind(ty) {
            TyKind::Null => (Ty::VOID, Some(Absence::Null)),
            TyKind::Void => (Ty::VOID, Some(Absence::Undefined)),
            TyKind::Union(span) => {
                let mut defined = std::mem::take(&mut self.scratch);
                defined.clear();
                let mut absence = None;
                for &member in self.union_slice(span) {
                    match member {
                        Ty::NULL => {
                            absence = Absence::join_optional(absence, Some(Absence::Null));
                        }
                        Ty::VOID => {
                            absence = Absence::join_optional(absence, Some(Absence::Undefined));
                        }
                        other => defined.push(other),
                    }
                }
                let inner = if defined.is_empty() {
                    Ty::VOID
                } else {
                    self.intern_sorted_union(&defined)
                };
                self.scratch = defined;
                (inner, absence)
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
        for &member in self.union_slice(span) {
            let drop = (member == Ty::NULL && excluded.admits_null())
                || (member == Ty::VOID && excluded.admits_undefined());
            if !drop {
                kept.push(member);
            }
        }
        let result = self.intern_sorted_union(&kept);
        self.scratch = kept;
        result
    }

    fn any_child(&self, ty: Ty, f: &mut impl FnMut(Ty) -> bool) -> bool {
        match self.kind(ty) {
            TyKind::Array(inner) | TyKind::Promise(inner) => f(inner),
            TyKind::Tuple(s) => self.tuple_slice(s).iter().copied().any(f),
            TyKind::Union(s) => self.union_slice(s).iter().copied().any(f),
            TyKind::Generic { args, .. } => self.generic_slice(args).iter().copied().any(f),
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

    /// Whether `ty` contains an unresolved type after resolving `Cell` locations.
    pub(crate) fn is_open_with(
        &self,
        ty: Ty,
        locations: &[Ty],
        dict_reads: &super::bitset::BitSet,
    ) -> bool {
        let mut visiting = Vec::new();
        self.is_open_with_rec(ty, locations, dict_reads, &mut visiting)
    }

    fn is_open_with_rec(
        &self,
        ty: Ty,
        locations: &[Ty],
        dict_reads: &super::bitset::BitSet,
        visiting: &mut Vec<LocId>,
    ) -> bool {
        match self.kind(ty) {
            TyKind::Bottom | TyKind::Open | TyKind::TypeVar(_) => true,
            TyKind::Cell { loc, kind } => {
                let Some(&elem) = locations.get(loc.0 as usize) else {
                    return true;
                };
                if kind == CellKind::Dict {
                    return !self.is_valid_dict_value(elem, dict_reads.contains(loc.0 as usize));
                }
                if visiting.contains(&loc) {
                    return true;
                }
                visiting.push(loc);
                let open = self.is_open_with_rec(elem, locations, dict_reads, visiting);
                visiting.pop();
                open
            }
            _ => self.any_child(ty, &mut |c| {
                self.is_open_with_rec(c, locations, dict_reads, visiting)
            }),
        }
    }

    /// Whether `ty` references `Cell(target)` directly or through nested cells/types.
    pub(crate) fn contains_cell(&self, ty: Ty, target: LocId, locations: &[Ty]) -> bool {
        let mut visiting = Vec::new();
        self.contains_cell_rec(ty, target, locations, &mut visiting)
    }

    fn contains_cell_rec(
        &self,
        ty: Ty,
        target: LocId,
        locations: &[Ty],
        visiting: &mut Vec<LocId>,
    ) -> bool {
        if let TyKind::Cell { loc, .. } = self.kind(ty) {
            if loc == target {
                return true;
            }
            if visiting.contains(&loc) {
                return false;
            }
            let Some(&elem) = locations.get(loc.0 as usize) else {
                return false;
            };
            visiting.push(loc);
            let hit = self.contains_cell_rec(elem, target, locations, visiting);
            visiting.pop();
            return hit;
        }
        self.any_child(ty, &mut |c| {
            self.contains_cell_rec(c, target, locations, visiting)
        })
    }

    /// Intern a boundary `HirType` into a dense [`Ty`].
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
            HirType::Array(inner) => {
                let inner = self.intern_hir(ctx, inner, explicit);
                self.array(inner)
            }
            HirType::Promise(inner) => {
                let inner = self.intern_hir(ctx, inner, explicit);
                self.promise(inner)
            }
            HirType::Tuple(items) => {
                let interned: Vec<Ty> = items
                    .iter()
                    .map(|item| self.intern_hir(ctx, item, explicit))
                    .collect();
                self.tuple(&interned)
            }
            HirType::Union(members) => {
                let interned: Vec<Ty> = members
                    .iter()
                    .map(|m| self.intern_hir(ctx, m, explicit))
                    .collect();
                self.union(&interned)
            }
            HirType::Generic { base, type_args } => {
                let base_id = shapes::TypeId::from_owned(ctx, HirType::Named(base.clone()));
                let args: Vec<Ty> = type_args
                    .iter()
                    .map(|arg| self.intern_hir(ctx, arg, explicit))
                    .collect();
                self.generic(base_id, &args)
            }
            HirType::Object(obj) => {
                let mut props: Vec<ObjectProp> = Vec::with_capacity(obj.properties.len());
                if let Some(order) = &obj.property_order {
                    for name in order {
                        if let Some(info) = obj.properties.get(name) {
                            let name_id = self.intern_name(name);
                            let prop_ty = self.intern_hir(ctx, &info.ty, explicit);
                            props.push(ObjectProp {
                                name: name_id,
                                ty: prop_ty,
                                optional: info.optional,
                                readonly: info.readonly,
                            });
                        }
                    }
                } else {
                    let mut keys: Vec<&String> = obj.properties.keys().collect();
                    keys.sort_unstable();
                    for name in keys {
                        if let Some(info) = obj.properties.get(name) {
                            let name_id = self.intern_name(name);
                            let prop_ty = self.intern_hir(ctx, &info.ty, explicit);
                            props.push(ObjectProp {
                                name: name_id,
                                ty: prop_ty,
                                optional: info.optional,
                                readonly: info.readonly,
                            });
                        }
                    }
                }
                let index = obj
                    .index_signature
                    .as_deref()
                    .map(|idx| self.intern_hir(ctx, idx, explicit));
                self.object(&props, index)
            }
        }
    }

    /// Materialize a final [`HirType`] at the emission boundary, resolving mutable empty cells.
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
            visiting_cells: Vec::new(),
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
        self.materialize_hir_with(ctx, ty, &[], &empty, class_name)
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

    /// Substitute bound type parameters in `ty` without cloning `HirType` trees.
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
                if let HirType::TypeVar(name) | HirType::Named(name) = id.hir(ctx) {
                    for &(param_id, bound) in bindings {
                        if matches!(param_id.hir(ctx), HirType::Named(p) | HirType::TypeVar(p) if p == name)
                        {
                            return bound;
                        }
                    }
                }
                ty
            }
            TyKind::Array(inner) => {
                let next = self.substitute(ctx, inner, bindings);
                if next == inner { ty } else { self.array(next) }
            }
            TyKind::Promise(inner) => {
                let next = self.substitute(ctx, inner, bindings);
                if next == inner {
                    ty
                } else {
                    self.promise(next)
                }
            }
            TyKind::Tuple(span) | TyKind::Union(span) | TyKind::Generic { args: span, .. } => {
                let kind = self.kind(ty);
                let items: Vec<Ty> = match kind {
                    TyKind::Tuple(_) => self.tuple_slice(span).to_vec(),
                    TyKind::Union(_) => self.union_slice(span).to_vec(),
                    _ => self.generic_slice(span).to_vec(),
                };
                let mut changed = false;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let next = self.substitute(ctx, item, bindings);
                    changed |= next != item;
                    out.push(next);
                }
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
                let mut items: Vec<ObjectProp> = self.object_slice(props).to_vec();
                let mut changed = false;
                for prop in &mut items {
                    let next = self.substitute(ctx, prop.ty, bindings);
                    if next != prop.ty {
                        prop.ty = next;
                        changed = true;
                    }
                }
                let next_idx = index.map(|idx| {
                    let next = self.substitute(ctx, idx, bindings);
                    changed |= next != idx;
                    next
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

    /// Unify a declared parameter type `pattern` against an actual argument type `actual` to bind
    /// explicit generic type parameters.
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
                    && let Some(&tp) = type_params.iter().find(|tp| {
                        matches!(tp.hir(ctx), HirType::Named(p) | HirType::TypeVar(p) if p == name)
                    })
                    && !bindings.iter().any(|(k, _)| *k == tp)
                {
                    bindings.push((tp, actual));
                }
            }
            (TyKind::Array(p), TyKind::Array(a)) | (TyKind::Promise(p), TyKind::Promise(a)) => {
                self.unify(ctx, p, a, type_params, bindings);
            }
            (TyKind::Tuple(p), TyKind::Tuple(a)) => {
                for (&pt, &at) in self.tuple_slice(p).iter().zip(self.tuple_slice(a)) {
                    self.unify(ctx, pt, at, type_params, bindings);
                }
            }
            (
                TyKind::Generic { base: pb, args: pa },
                TyKind::Generic { base: ab, args: aa },
            ) if pb == ab => {
                for (&pt, &at) in self.generic_slice(pa).iter().zip(self.generic_slice(aa)) {
                    self.unify(ctx, pt, at, type_params, bindings);
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
                    if let Some(actual_prop) = a_slice.iter().find(|p| p.name == prop.name) {
                        self.unify(ctx, prop.ty, actual_prop.ty, type_params, bindings);
                    }
                }
                if let (Some(pt), Some(at)) = (pi, ai) {
                    self.unify(ctx, pt, at, type_params, bindings);
                }
            }
            _ => {}
        }
    }

    /// Whether `actual` is assignable to `expected` under explicit annotation checks.
    pub(crate) fn accepts(&self, ctx: &Context, expected: Ty, actual: Ty) -> bool {
        if expected == actual || actual == Ty::NEVER || expected == Ty::DYNAMIC {
            return true;
        }
        if let TyKind::Union(span) = self.kind(actual) {
            return self
                .union_slice(span)
                .iter()
                .all(|&member| self.accepts(ctx, expected, member));
        }
        if let TyKind::Union(span) = self.kind(expected) {
            return self
                .union_slice(span)
                .iter()
                .any(|&member| self.accepts(ctx, member, actual));
        }
        match (self.kind(expected), self.kind(actual)) {
            (TyKind::Number, TyKind::Int32) => true,
            (TyKind::String, TyKind::Leaf(id)) => {
                matches!(id.hir(ctx), HirType::StringLiteral(_))
            }
            _ => false,
        }
    }

    /// Whether a value written to a mutable aggregate cell contains an unresolved or nested-mixed type.
    pub(crate) fn has_nested_unresolved(&self, ty: Ty, nested: bool) -> bool {
        match self.kind(ty) {
            TyKind::Bottom | TyKind::Open | TyKind::Dynamic | TyKind::TypeVar(_) => true,
            TyKind::Tuple(span) => {
                let items = self.tuple_slice(span);
                (nested && items.windows(2).any(|w| w[0] != w[1]))
                    || items.iter().any(|&t| self.has_nested_unresolved(t, true))
            }
            TyKind::Union(span) => {
                let members = self.union_slice(span);
                let multi_defined = nested
                    && members
                        .iter()
                        .filter(|&&m| m != Ty::NULL && m != Ty::VOID)
                        .count()
                        > 1;
                multi_defined
                    || members
                        .iter()
                        .any(|&t| self.has_nested_unresolved(t, nested))
            }
            _ => self.any_child(ty, &mut |c| self.has_nested_unresolved(c, true)),
        }
    }

    /// Whether `needle` occurs structurally inside `haystack` (for growing recursive type args).
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
    visiting_cells: Vec<LocId>,
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
        self.visiting_cells.push(loc);
        let hir = self.run(ty);
        self.visiting_cells.pop();
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
            TyKind::Array(inner) => HirType::Array(Box::new(self.run(inner))),
            TyKind::Promise(inner) => HirType::Promise(Box::new(self.run(inner))),
            TyKind::Cell { loc, kind } => {
                if self.visiting_cells.contains(&loc) {
                    return match kind {
                        CellKind::Array | CellKind::LocalArray => {
                            HirType::Array(Box::new(HirType::Any))
                        }
                        CellKind::Dict => Self::empty_object(None),
                    };
                }
                debug_assert!((loc.0 as usize) < self.locations.len());
                let elem = self
                    .locations
                    .get(loc.0 as usize)
                    .copied()
                    .unwrap_or(Ty::OPEN);
                match kind {
                    CellKind::Array | CellKind::LocalArray => {
                        HirType::Array(Box::new(self.run_cell(loc, elem)))
                    }
                    CellKind::Dict => {
                        let idx = self
                            .table
                            .is_valid_dict_value(elem, self.dict_reads.contains(loc.0 as usize))
                            .then(|| Box::new(self.run_cell(loc, elem)));
                        Self::empty_object(idx)
                    }
                }
            }
            TyKind::Tuple(span) => HirType::Tuple(
                self.table
                    .tuple_slice(span)
                    .iter()
                    .map(|&t| self.run(t))
                    .collect(),
            ),
            TyKind::Union(span) => {
                let mut members = Vec::with_capacity(span.len as usize);
                for &t in self.table.union_slice(span) {
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
                    HirType::Named(name) => name.clone(),
                    _ => String::new(),
                },
                type_args: self
                    .table
                    .generic_slice(args)
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
                debug_assert!(name.is_some(), "missing class instance name for {inst:?}");
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
