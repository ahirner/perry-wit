//! Dense interned types over `shapes::TypeId`, canonical sorted unions, and monotone joins.
//!
//! `HirType` trees cross this boundary only on interning (`from_hir`) and emission (`to_hir`).
//! Everything inside the engine compares and joins `Copy` `Ty` indices with zero `HirType` clones.
use crate::{Absence, shapes};
use perry_hir::types::{ObjectType, PropertyInfo, Type as HirType};
use pliron::context::Context;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

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
    pub(crate) const EMPTY: Self = Self { start: 0, len: 0 };

    pub(crate) fn range(self) -> std::ops::Range<usize> {
        let start = self.start as usize;
        start..start.saturating_add(self.len as usize)
    }

    pub(crate) fn is_empty(self) -> bool {
        self.len == 0
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

const EMPTY_BUCKET: u32 = u32::MAX;

/// Flat intrusive hash table for interning dense indices without per-bucket heap allocations.
struct FlatHashIndex {
    buckets: Vec<u32>,
    next: Vec<u32>,
    hashes: Vec<u64>,
}

impl FlatHashIndex {
    fn with_capacity(cap: usize) -> Self {
        let bucket_count = cap.next_power_of_two().max(64);
        Self {
            buckets: vec![EMPTY_BUCKET; bucket_count],
            next: Vec::with_capacity(cap),
            hashes: Vec::with_capacity(cap),
        }
    }

    fn find(&self, hash: u64, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        let mask = self.buckets.len().checked_sub(1)?;
        let mut idx = *self.buckets.get((hash as usize) & mask)?;
        while idx != EMPTY_BUCKET {
            if self.hashes.get(idx as usize).copied() == Some(hash) && eq(idx) {
                return Some(idx);
            }
            idx = self.next.get(idx as usize).copied().unwrap_or(EMPTY_BUCKET);
        }
        None
    }

    fn insert(&mut self, id: u32, hash: u64) {
        let id_usize = id as usize;
        if id_usize >= self.next.len() {
            self.next.resize(id_usize + 1, EMPTY_BUCKET);
            self.hashes.resize(id_usize + 1, 0);
        }
        if let Some(slot) = self.hashes.get_mut(id_usize) {
            *slot = hash;
        }
        if self.next.len() * 4 > self.buckets.len() * 3 {
            self.rehash();
        }
        let mask = self.buckets.len().saturating_sub(1);
        let bucket = (hash as usize) & mask;
        let head = self.buckets.get(bucket).copied().unwrap_or(EMPTY_BUCKET);
        if let Some(slot) = self.next.get_mut(id_usize) {
            *slot = head;
        }
        if let Some(slot) = self.buckets.get_mut(bucket) {
            *slot = id;
        }
    }

    fn rehash(&mut self) {
        let new_len = self.buckets.len().saturating_mul(2).max(64);
        self.buckets.clear();
        self.buckets.resize(new_len, EMPTY_BUCKET);
        let mask = new_len.saturating_sub(1);
        for id in 0..self.next.len() {
            let hash = self.hashes.get(id).copied().unwrap_or(0);
            let bucket = (hash as usize) & mask;
            let head = self.buckets.get(bucket).copied().unwrap_or(EMPTY_BUCKET);
            if let Some(slot) = self.next.get_mut(id) {
                *slot = head;
            }
            if let Some(slot) = self.buckets.get_mut(bucket) {
                *slot = id as u32;
            }
        }
    }
}

fn hash_bytes(tag: u8, bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut h);
    bytes.hash(&mut h);
    h.finish()
}

fn hash_value<T: Hash>(tag: u8, value: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut h);
    value.hash(&mut h);
    h.finish()
}

pub(crate) struct TypeTable {
    kinds: Vec<TyKind>,
    index: FlatHashIndex,
    union_members: Vec<Ty>,
    tuple_items: Vec<Ty>,
    generic_args: Vec<Ty>,
    object_props: Vec<ObjectProp>,
    names: Vec<String>,
    name_index: FlatHashIndex,
    scratch: Vec<Ty>,
}

impl TypeTable {
    pub(crate) fn new() -> Self {
        let mut table = Self {
            kinds: Vec::with_capacity(64),
            index: FlatHashIndex::with_capacity(64),
            union_members: Vec::new(),
            tuple_items: Vec::new(),
            generic_args: Vec::new(),
            object_props: Vec::new(),
            names: Vec::new(),
            name_index: FlatHashIndex::with_capacity(32),
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
            let hash = hash_value(0, &kind);
            table.kinds.push(kind);
            table.index.insert(i as u32, hash);
        }
        table
    }

    pub(crate) fn len(&self) -> usize {
        self.kinds.len()
    }

    pub(crate) fn kind(&self, ty: Ty) -> TyKind {
        self.kinds.get(ty.index()).copied().unwrap_or(TyKind::Open)
    }

    pub(crate) fn intern_name(&mut self, name: &str) -> NameId {
        let hash = hash_bytes(1, name.as_bytes());
        if let Some(id) = self.name_index.find(hash, |idx| {
            self.names.get(idx as usize).map(String::as_str) == Some(name)
        }) {
            return NameId(id);
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.name_index.insert(id, hash);
        NameId(id)
    }

    pub(crate) fn name_str(&self, id: NameId) -> &str {
        self.names.get(id.0 as usize).map_or("", String::as_str)
    }

    pub(crate) fn union_slice(&self, span: TySpan) -> &[Ty] {
        self.union_members.get(span.range()).unwrap_or(&[])
    }

    pub(crate) fn tuple_slice(&self, span: TySpan) -> &[Ty] {
        self.tuple_items.get(span.range()).unwrap_or(&[])
    }

    pub(crate) fn generic_slice(&self, span: TySpan) -> &[Ty] {
        self.generic_args.get(span.range()).unwrap_or(&[])
    }

    pub(crate) fn object_slice(&self, span: TySpan) -> &[ObjectProp] {
        self.object_props.get(span.range()).unwrap_or(&[])
    }

    fn intern_simple(&mut self, kind: TyKind) -> Ty {
        let hash = hash_value(0, &kind);
        if let Some(id) = self
            .index
            .find(hash, |idx| self.kinds.get(idx as usize) == Some(&kind))
        {
            return Ty(id);
        }
        let id = self.kinds.len() as u32;
        self.kinds.push(kind);
        self.index.insert(id, hash);
        Ty(id)
    }

    pub(crate) fn leaf(&mut self, id: shapes::TypeId) -> Ty {
        self.intern_simple(TyKind::Leaf(id))
    }

    pub(crate) fn named(&mut self, ctx: &Context, name: &str) -> Ty {
        let id = shapes::TypeId::from_owned(ctx, HirType::Named(name.to_owned()));
        self.leaf(id)
    }

    pub(crate) fn string_literal(&mut self, ctx: &Context, value: &str) -> Ty {
        let id = shapes::TypeId::from_owned(ctx, HirType::StringLiteral(value.to_owned()));
        self.leaf(id)
    }

    pub(crate) fn type_var(&mut self, id: shapes::TypeId) -> Ty {
        self.intern_simple(TyKind::TypeVar(id))
    }

    pub(crate) fn array(&mut self, element: Ty) -> Ty {
        self.intern_simple(TyKind::Array(element))
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
        let hash = hash_value(2, &items);
        if let Some(id) = self.index.find(hash, |idx| {
            matches!(
                self.kinds.get(idx as usize),
                Some(TyKind::Tuple(span)) if self.tuple_items.get(span.range()) == Some(items)
            )
        }) {
            return Ty(id);
        }
        let start = self.tuple_items.len() as u32;
        self.tuple_items.extend_from_slice(items);
        let span = TySpan {
            start,
            len: items.len() as u32,
        };
        let id = self.kinds.len() as u32;
        self.kinds.push(TyKind::Tuple(span));
        self.index.insert(id, hash);
        Ty(id)
    }

    pub(crate) fn generic(&mut self, base: shapes::TypeId, args: &[Ty]) -> Ty {
        let hash = hash_value(3, &(base, args));
        if let Some(id) = self.index.find(hash, |idx| {
            matches!(
                self.kinds.get(idx as usize),
                Some(TyKind::Generic { base: b, args: span })
                    if *b == base && self.generic_args.get(span.range()) == Some(args)
            )
        }) {
            return Ty(id);
        }
        let start = self.generic_args.len() as u32;
        self.generic_args.extend_from_slice(args);
        let span = TySpan {
            start,
            len: args.len() as u32,
        };
        let id = self.kinds.len() as u32;
        self.kinds.push(TyKind::Generic { base, args: span });
        self.index.insert(id, hash);
        Ty(id)
    }

    pub(crate) fn object(&mut self, props: &mut [ObjectProp], index: Option<Ty>) -> Ty {
        props.sort_unstable_by_key(|p| p.name);
        let hash = hash_value(4, &(props as &[ObjectProp], index));
        if let Some(id) = self.index.find(hash, |idx| {
            matches!(
                self.kinds.get(idx as usize),
                Some(TyKind::Object { props: span, index: i })
                    if *i == index && self.object_props.get(span.range()) == Some(props as &[ObjectProp])
            )
        }) {
            return Ty(id);
        }
        let start = self.object_props.len() as u32;
        self.object_props.extend_from_slice(props);
        let span = TySpan {
            start,
            len: props.len() as u32,
        };
        let id = self.kinds.len() as u32;
        self.kinds.push(TyKind::Object { props: span, index });
        self.index.insert(id, hash);
        Ty(id)
    }

    /// Intern a canonical sorted, flattened, deduplicated union.
    pub(crate) fn union(&mut self, members: &[Ty]) -> Ty {
        let mut flat = std::mem::take(&mut self.scratch);
        flat.clear();
        for &member in members {
            self.push_union_member(&mut flat, member);
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
            slice => {
                let hash = hash_value(5, &slice);
                if let Some(id) = self.index.find(hash, |idx| {
                    matches!(
                        self.kinds.get(idx as usize),
                        Some(TyKind::Union(span)) if self.union_members.get(span.range()) == Some(slice)
                    )
                }) {
                    return Ty(id);
                }
                let start = self.union_members.len() as u32;
                self.union_members.extend_from_slice(slice);
                let span = TySpan {
                    start,
                    len: slice.len() as u32,
                };
                let id = self.kinds.len() as u32;
                self.kinds.push(TyKind::Union(span));
                self.index.insert(id, hash);
                Ty(id)
            }
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
        self.union(&[a, b])
    }

    /// Normalize a value written into storage (widening `Int32 -> Number`, `StringLiteral -> String`,
    /// and 32-bit WIT integer aliases to `Number`, matching #70's storage rules).
    pub(crate) fn normalize_for_storage(
        &mut self,
        ctx: &Context,
        decls: &shapes::Declarations<'_>,
        ty: Ty,
    ) -> Ty {
        match self.kind(ty) {
            TyKind::Int32 => Ty::NUMBER,
            TyKind::Leaf(id) => match id.hir(ctx) {
                HirType::StringLiteral(_) => Ty::STRING,
                HirType::Named(name) => match decls.integer_carrier(name) {
                    Some(HirType::Number) => Ty::NUMBER,
                    _ => ty,
                },
                _ => ty,
            },
            TyKind::Union(span) => {
                let mut flat = std::mem::take(&mut self.scratch);
                flat.clear();
                let count = span.len as usize;
                let start = span.start as usize;
                for offset in 0..count {
                    let Some(&member) = self.union_members.get(start + offset) else {
                        break;
                    };
                    let norm = match self.kind(member) {
                        TyKind::Int32 => Ty::NUMBER,
                        TyKind::Leaf(id) => match id.hir(ctx) {
                            HirType::StringLiteral(_) => Ty::STRING,
                            HirType::Named(name) => match decls.integer_carrier(name) {
                                Some(HirType::Number) => Ty::NUMBER,
                                _ => member,
                            },
                            _ => member,
                        },
                        _ => member,
                    };
                    if norm != Ty::BOTTOM && norm != Ty::NEVER {
                        flat.push(norm);
                    }
                }
                flat.sort_unstable();
                flat.dedup();
                let result = self.intern_sorted_union(&flat);
                self.scratch = flat;
                result
            }
            _ => ty,
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

    pub(crate) fn is_open(&self, ty: Ty) -> bool {
        match self.kind(ty) {
            TyKind::Bottom | TyKind::Open | TyKind::TypeVar(_) => true,
            TyKind::Array(inner) | TyKind::Promise(inner) => self.is_open(inner),
            TyKind::Tuple(span) => self.tuple_slice(span).iter().any(|&t| self.is_open(t)),
            TyKind::Union(span) => self.union_slice(span).iter().any(|&t| self.is_open(t)),
            TyKind::Generic { args, .. } => {
                self.generic_slice(args).iter().any(|&t| self.is_open(t))
            }
            TyKind::Object { props, index } => {
                index.is_some_and(|t| self.is_open(t))
                    || self.object_slice(props).iter().any(|p| self.is_open(p.ty))
            }
            _ => false,
        }
    }

    pub(crate) fn contains_type_var(&self, ty: Ty) -> bool {
        match self.kind(ty) {
            TyKind::TypeVar(_) => true,
            TyKind::Array(inner) | TyKind::Promise(inner) => self.contains_type_var(inner),
            TyKind::Tuple(span) => self
                .tuple_slice(span)
                .iter()
                .any(|&t| self.contains_type_var(t)),
            TyKind::Union(span) => self
                .union_slice(span)
                .iter()
                .any(|&t| self.contains_type_var(t)),
            TyKind::Generic { args, .. } => self
                .generic_slice(args)
                .iter()
                .any(|&t| self.contains_type_var(t)),
            TyKind::Object { props, index } => {
                index.is_some_and(|t| self.contains_type_var(t))
                    || self
                        .object_slice(props)
                        .iter()
                        .any(|p| self.contains_type_var(p.ty))
            }
            _ => false,
        }
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
                let mut props: Vec<ObjectProp> = obj
                    .properties
                    .iter()
                    .map(|(name, info)| {
                        let name = self.intern_name(name);
                        let prop_ty = self.intern_hir(ctx, &info.ty, explicit);
                        ObjectProp {
                            name,
                            ty: prop_ty,
                            optional: info.optional,
                            readonly: info.readonly,
                        }
                    })
                    .collect();
                let index = obj
                    .index_signature
                    .as_deref()
                    .map(|idx| self.intern_hir(ctx, idx, explicit));
                self.object(&mut props, index)
            }
        }
    }

    /// Materialize a final [`HirType`] at the emission boundary.
    pub(crate) fn materialize_hir(
        &self,
        ctx: &Context,
        ty: Ty,
        class_name: &impl Fn(ClassInstId) -> Option<String>,
    ) -> HirType {
        match self.kind(ty) {
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
            TyKind::Leaf(id) | TyKind::TypeVar(id) => id.hir(ctx).clone(),
            TyKind::Array(inner) => {
                HirType::Array(Box::new(self.materialize_hir(ctx, inner, class_name)))
            }
            TyKind::Promise(inner) => {
                HirType::Promise(Box::new(self.materialize_hir(ctx, inner, class_name)))
            }
            TyKind::Tuple(span) => HirType::Tuple(
                self.tuple_slice(span)
                    .iter()
                    .map(|&t| self.materialize_hir(ctx, t, class_name))
                    .collect(),
            ),
            TyKind::Union(span) => HirType::Union(
                self.union_slice(span)
                    .iter()
                    .map(|&t| self.materialize_hir(ctx, t, class_name))
                    .collect(),
            ),
            TyKind::Generic { base, args } => {
                let base_name = match base.hir(ctx) {
                    HirType::Named(name) => name.clone(),
                    _ => String::new(),
                };
                HirType::Generic {
                    base: base_name,
                    type_args: self
                        .generic_slice(args)
                        .iter()
                        .map(|&t| self.materialize_hir(ctx, t, class_name))
                        .collect(),
                }
            }
            TyKind::Object { props, index } => {
                let mut properties = HashMap::with_capacity(props.len as usize);
                for prop in self.object_slice(props) {
                    properties.insert(
                        self.name_str(prop.name).to_owned(),
                        PropertyInfo {
                            ty: self.materialize_hir(ctx, prop.ty, class_name),
                            optional: prop.optional,
                            readonly: prop.readonly,
                        },
                    );
                }
                HirType::Object(ObjectType {
                    name: None,
                    properties,
                    property_order: None,
                    index_signature: index
                        .map(|t| Box::new(self.materialize_hir(ctx, t, class_name))),
                })
            }
            TyKind::ClassInst(inst) => HirType::Named(class_name(inst).unwrap_or_default()),
            TyKind::Func(_) => HirType::Function(perry_hir::types::FunctionType {
                params: Vec::new(),
                return_type: Box::new(HirType::Void),
                is_async: false,
                is_generator: false,
            }),
        }
    }
}
