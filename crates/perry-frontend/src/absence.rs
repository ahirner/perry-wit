/// Absence states admitted by a source type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Absence {
    Null,
    Undefined,
    Both,
}
impl Absence {
    pub const fn admits_null(self) -> bool {
        matches!(self, Self::Null | Self::Both)
    }
    pub const fn admits_undefined(self) -> bool {
        matches!(self, Self::Undefined | Self::Both)
    }
    /// The states admitted by either side.
    pub fn join(self, other: Self) -> Self {
        if self == other { self } else { Self::Both }
    }
    /// [`Self::join`] where `None` admits no absence.
    pub fn join_optional(left: Option<Self>, right: Option<Self>) -> Option<Self> {
        left.into_iter().chain(right).reduce(Self::join)
    }
}
