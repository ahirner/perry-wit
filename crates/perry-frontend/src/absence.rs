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
}
pub fn merge_absence(left: Option<Absence>, right: Option<Absence>) -> Option<Absence> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) if left == right => Some(left),
        (Some(_), Some(_)) => Some(Absence::Both),
    }
}
