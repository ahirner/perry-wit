//! Source collection iterator projections.
use crate::source::CollectionKind;
use perry_mir::RecordKind;

/// `keys()`, `values()` or `entries()`, the iterator methods of `Map`, `Set` and arrays.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IteratorMethod {
    Keys,
    Values,
    Entries,
}

impl CollectionKind {
    pub(crate) fn from_type_name(name: &str) -> Option<Self> {
        match name {
            "Map" | "ReadonlyMap" => Some(Self::Map),
            "Set" | "ReadonlySet" => Some(Self::Set),
            _ => None,
        }
    }

    pub(crate) fn type_fields<T: Copy>(self, args: &[T]) -> Option<[T; 2]> {
        match (self, args) {
            (Self::Map, &[k, v]) => Some([k, v]),
            (Self::Set, &[v]) => Some([v; 2]),
            _ => None,
        }
    }

    pub(crate) fn default_view(self) -> CollectionViewKind {
        match self {
            Self::Map => CollectionViewKind::MapEntries,
            Self::Set => CollectionViewKind::SetValues,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Yield<T> {
    Item(T),
    Pair(T, T),
}

impl<T> Yield<T> {
    pub(crate) fn materialize<R>(
        self,
        item: impl FnOnce(T) -> R,
        pair: impl FnOnce([T; 2]) -> R,
    ) -> R {
        match self {
            Self::Item(value) => item(value),
            Self::Pair(a, b) => pair([a, b]),
        }
    }
}

impl<T: Copy + PartialEq> Yield<T> {
    pub(crate) fn try_map<U: Copy, E>(
        self,
        mut field: impl FnMut(T) -> Result<U, E>,
    ) -> Result<Yield<U>, E> {
        Ok(match self {
            Self::Item(a) => Yield::Item(field(a)?),
            Self::Pair(a, b) => {
                let first = field(a)?;
                Yield::Pair(first, if a == b { first } else { field(b)? })
            }
        })
    }
}

impl IteratorMethod {
    pub(crate) fn project<T>(self, [key, value]: [T; 2]) -> Yield<T> {
        match self {
            Self::Keys => Yield::Item(key),
            Self::Values => Yield::Item(value),
            Self::Entries => Yield::Pair(key, value),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Keys => "keys",
            Self::Values => "values",
            Self::Entries => "entries",
        }
    }
}

// The source name, MIR record role and iterator projection identify the same live view.
macro_rules! collection_views {
    ($( $source:ident $($method:ident)|+ => $view:ident : $form:ident($($slot:literal),+) ),+ $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) enum CollectionViewKind { $($view),+ }

        impl IteratorMethod {
            pub(crate) fn view(self, source: CollectionKind) -> CollectionViewKind {
                match (source, self) {
                    $((CollectionKind::$source, $(Self::$method)|+) => CollectionViewKind::$view,)+
                }
            }
        }

        impl CollectionViewKind {
            pub(crate) fn project<T: Copy>(self, fields: [T; 2]) -> Yield<T> {
                match self { $(Self::$view => Yield::$form($(fields[$slot]),+),)+ }
            }
            pub(crate) fn source(self) -> CollectionKind {
                match self { $(Self::$view => CollectionKind::$source,)+ }
            }
            pub(crate) fn type_name(self) -> &'static str {
                match self { $(Self::$view => stringify!($view),)+ }
            }
            pub(crate) fn from_type_name(name: &str) -> Option<Self> {
                match name { $(stringify!($view) => Some(Self::$view),)+ _ => None }
            }
            pub(crate) fn record_kind(self) -> RecordKind {
                match self { $(Self::$view => RecordKind::$view,)+ }
            }
        }
    };

}

collection_views! {
    Map Keys => MapKeys: Item(0),
    Map Values => MapValues: Item(1),
    Map Entries => MapEntries: Pair(0, 1),
    Set Keys | Values => SetValues: Item(0),
    Set Entries => SetEntries: Pair(0, 0),
}
