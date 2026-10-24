//! Source collection iterator projections.
/// `keys()`, `values()` or `entries()`, the iterator methods of `Map`, `Set` and arrays.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IteratorMethod {
    Keys,
    Values,
    Entries,
}

impl IteratorMethod {
    pub fn name(self) -> &'static str {
        match self {
            Self::Keys => "keys",
            Self::Values => "values",
            Self::Entries => "entries",
        }
    }

    /// The live view this method returns on a `Map`.
    pub fn map_view(self) -> CollectionViewKind {
        match self {
            Self::Keys => CollectionViewKind::MapKeys,
            Self::Values => CollectionViewKind::MapValues,
            Self::Entries => CollectionViewKind::MapEntries,
        }
    }

    /// The live view this method returns on a `Set`, whose keys are its values.
    pub fn set_view(self) -> CollectionViewKind {
        match self {
            Self::Keys | Self::Values => CollectionViewKind::SetValues,
            Self::Entries => CollectionViewKind::SetEntries,
        }
    }
}

/// A live iterator view over a `Map` or `Set` box.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CollectionViewKind {
    MapKeys,
    MapValues,
    MapEntries,
    SetValues,
    SetEntries,
}

impl CollectionViewKind {
    pub const ALL: [Self; 5] = [
        Self::MapKeys,
        Self::MapValues,
        Self::MapEntries,
        Self::SetValues,
        Self::SetEntries,
    ];

    /// The generic name inference gives the view (`MapKeys<K, V>`, `SetValues<T>`).
    pub fn type_name(self) -> &'static str {
        match self {
            Self::MapKeys => "MapKeys",
            Self::MapValues => "MapValues",
            Self::MapEntries => "MapEntries",
            Self::SetValues => "SetValues",
            Self::SetEntries => "SetEntries",
        }
    }

    pub fn from_type_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.type_name() == name)
    }
}
