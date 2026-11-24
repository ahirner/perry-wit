//! Index-typed vectors, dense IDs, and canonical type keys.
use crate::shapes;

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
