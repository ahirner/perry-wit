//! Index-typed vectors, dense IDs, and canonical type keys.
use crate::shapes;
use perry_hir::types::LocalId;
use std::collections::HashMap;
use std::hash::Hash;

mod sealed {
    pub(crate) trait MintIdx: Copy + Eq {
        fn from_usize(n: usize) -> Self;
    }
    pub(crate) trait MintSpan: Copy + Eq {
        fn from_range(start: usize, len: usize) -> Self;
        fn range(self) -> std::ops::Range<usize>;
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

impl<I, T: PartialEq> PartialEq for IdxVec<I, T> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<I, T: Eq> Eq for IdxVec<I, T> {}

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
    pub(crate) fn map<U>(&self, f: impl FnMut(&T) -> U) -> IdxVec<I, U> {
        IdxVec {
            data: self.data.iter().map(f).collect(),
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
        &self.data[id.index()]
    }
}

impl<I: Idx, T> std::ops::IndexMut<I> for IdxVec<I, T> {
    fn index_mut(&mut self, id: I) -> &mut T {
        &mut self.data[id.index()]
    }
}

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
            pub(crate) struct $name(u32);
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

/// Append-only slice pool whose typed spans have private fields and are minted only by [`Pool`].
pub(crate) struct Pool<S, T> {
    data: Vec<T>,
    _marker: std::marker::PhantomData<fn(S) -> S>,
}

impl<S, T> Default for Pool<S, T> {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }
}

#[allow(private_bounds)]
impl<S: sealed::MintSpan, T> Pool<S, T> {
    pub(crate) fn extend_from_slice(&mut self, items: &[T]) -> S
    where
        T: Copy,
    {
        let start = self.data.len();
        self.data.extend_from_slice(items);
        S::from_range(start, items.len())
    }

    pub(crate) fn intern(&mut self, index: &mut HashMap<Box<[T]>, S>, items: &[T]) -> S
    where
        T: Copy + Eq + Hash,
    {
        if let Some(&span) = index.get(items) {
            return span;
        }
        let span = self.extend_from_slice(items);
        index.insert(items.into(), span);
        span
    }
}

#[allow(private_bounds)]
impl<S: sealed::MintSpan, T> std::ops::Index<S> for Pool<S, T> {
    type Output = [T];
    fn index(&self, span: S) -> &[T] {
        &self.data[span.range()]
    }
}

macro_rules! span_type {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
            pub(crate) struct $name {
                start: u32,
                len: u32,
            }
            impl sealed::MintSpan for $name {
                fn from_range(start: usize, len: usize) -> Self {
                    debug_assert!(u32::try_from(start).is_ok() && u32::try_from(len).is_ok(), "pool span overflow");
                    Self {
                        start: start as u32,
                        len: len as u32,
                    }
                }
                fn range(self) -> std::ops::Range<usize> {
                    let s = self.start as usize;
                    s..s + self.len as usize
                }
            }
        )*
    };
}

span_type!(
    TySpan,
    PropSpan,
    ParamLocSpan,
    FieldLocSpan,
    DispatchArmSpan,
    SiteSpan,
    CaptureSpan,
    CapturedLocalSpan,
    TypeParamSpan,
);

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
    Timer,
    Named(shapes::TypeName),
    Literal(shapes::TypeName),
    Signature {
        params: PropSpan,
        result: Ty,
        is_async: bool,
        is_generator: bool,
    },
    TypeVar(shapes::TypeName),
    Array(Ty),
    Cell {
        loc: LocId,
        kind: CellKind,
    },
    Promise(Ty),
    Tuple(TySpan),
    Union(TySpan),
    Generic {
        base: shapes::TypeName,
        args: TySpan,
    },
    Object {
        props: PropSpan,
        index: Option<Ty>,
    },
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
    pub(crate) const TIMER: Self = Self(12);
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct LocalFlags(u16);

impl LocalFlags {
    pub(crate) const INFERRED: Self = Self(1 << 0);
    pub(crate) const DEFERRED: Self = Self(1 << 1);
    pub(crate) const FOR_HEAD: Self = Self(1 << 2);
    pub(crate) const NULLISH_INIT: Self = Self(1 << 3);
    pub(crate) const REASSIGNED: Self = Self(1 << 4);
    pub(crate) const PARAM: Self = Self(1 << 5);
    pub(crate) const PARAM_ANNOTATED: Self = Self(1 << 6);
    pub(crate) const MODULE: Self = Self(1 << 7);
    pub(crate) const CAPTURED: Self = Self(1 << 8);
    pub(crate) const WRITTEN_FROM_CLOSURE: Self = Self(1 << 9);
    pub(crate) const FORWARD_CAPTURED: Self = Self(1 << 10);
    pub(crate) const MODULE_OR_PARAM: Self = Self(Self::MODULE.0 | Self::PARAM.0);
    pub(crate) const BOXED_TRIGGER: Self = Self(
        Self::REASSIGNED.0
            | Self::WRITTEN_FROM_CLOSURE.0
            | Self::DEFERRED.0
            | Self::FOR_HEAD.0
            | Self::FORWARD_CAPTURED.0,
    );
    pub(crate) const DEFERRED_OR_NULLISH: Self = Self(Self::DEFERRED.0 | Self::NULLISH_INIT.0);

    pub(crate) fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
    pub(crate) fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub(crate) fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) enum LocalStorage {
    #[default]
    None,
    Module(LocId),
    Frame(u32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalInfo {
    pub(crate) id: LocalId,
    pub(crate) owner: TemplateId,
    pub(crate) declared: Ty,
    pub(crate) storage: LocalStorage,
    pub(crate) flags: LocalFlags,
}

impl LocalInfo {
    pub(crate) fn module_loc(self) -> Option<LocId> {
        match self.storage {
            LocalStorage::Module(loc) => Some(loc),
            _ => None,
        }
    }
    pub(crate) fn frame_slot(self) -> Option<usize> {
        match self.storage {
            LocalStorage::Frame(slot) => Some(slot as usize),
            _ => None,
        }
    }
}
