//! Types of the `perry` dialect.
use pliron::{
    builtin::attributes::StringAttr,
    combine::{Parser, between, optional, token},
    common_traits::Verify,
    context::Context,
    derive::pliron_type,
    dict_key,
    identifier::Identifier,
    input_err_noloc,
    irfmt::parsers::{delimited_list_parser, location, spaced, type_parser},
    parsable::{IntoParseResult, Parsable, ParseResult, StateStream},
    printable::{self, Printable},
    result::Result,
    r#type::{Type, TypeHandle, TypedHandle},
};

mod kinds;
pub use kinds::*;
mod records;
pub use records::RecordLayouts;

/// `ty` as a `T` when it is one. Unlike [TypedHandle::from_handle], a mismatch does not print
/// both types into an error, so this is the type test for hot paths.
pub fn typed<T: Type>(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<T>> {
    if ty.deref(ctx).is::<T>() {
        TypedHandle::from_handle(ty, ctx).ok()
    } else {
        None
    }
}

/// Each reachable MIR storage type once, including roots and opaque record declarations.
pub fn nested_types(
    ctx: &Context,
    roots: impl IntoIterator<Item = TypeHandle>,
) -> impl Iterator<Item = TypeHandle> + '_ {
    let mut seen = std::collections::HashSet::new();
    let mut pending: Vec<_> = roots.into_iter().filter(|ty| seen.insert(*ty)).collect();
    std::iter::from_fn(move || {
        let ty = pending.pop()?;
        if let Some(children) = type_children(&*ty.deref(ctx)) {
            pending.extend(children.filter(|child| seen.insert(*child)));
        }
        Some(ty)
    })
}

/// Immediate storage types. A forward-declared record has unknown children.
pub(crate) fn type_children(ty: &dyn Type) -> Option<impl Iterator<Item = TypeHandle> + '_> {
    let fields = match ty.downcast_ref::<RecordType>() {
        Some(record) => record.fields.as_deref()?,
        None => &[],
    };
    let cases = ty
        .downcast_ref::<VariantType>()
        .map_or(&[][..], VariantType::cases);
    let payload = ty
        .downcast_ref::<ValueUnionType>()
        .map(ValueUnionType::variants)
        .or_else(|| ty.downcast_ref::<OptionalType>().map(OptionalType::value))
        .or_else(|| ty.downcast_ref::<ArrayType>().map(ArrayType::element))
        .or_else(|| ty.downcast_ref::<PromiseType>().map(PromiseType::value))
        .or_else(|| ty.downcast_ref::<SettledType>().map(SettledType::value))
        .or_else(|| ty.downcast_ref::<SlotType>().map(SlotType::value));
    Some(
        fields
            .iter()
            .map(|(_, field)| *field)
            .chain(cases.iter().filter_map(|(_, payload)| *payload))
            .chain(payload),
    )
}

macro_rules! record_kinds {
    ($($kind:ident => $name:literal),+ $(,)?) => {
        /// Logical record role, preserved when numeric storage is specialized.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum RecordKind { $($kind),+ }

        impl RecordKind {
            pub fn name(self) -> &'static str {
                match self { $(Self::$kind => $name),+ }
            }

            fn parse(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$kind),)+ _ => None }
            }
        }
    };
}

record_kinds! {
    Object => "object",
    Tuple => "tuple",
    DictionaryEntry => "dict_tuple",
    MapEntry => "map_tuple",
    SetEntry => "set_tuple",
    Map => "map_box",
    Set => "set_box",
    MapKeys => "map_keys_box",
    MapValues => "map_values_box",
    MapEntries => "map_entries_box",
    SetValues => "set_values_box",
    SetEntries => "set_entries_box",
}

/// Check whether a value can retain native storage, treating unknown dialect types conservatively.
pub(crate) fn is_reference(ctx: &Context, ty: TypeHandle) -> bool {
    if let Some(inner) = crate::optional::optional_value(ctx, ty) {
        return is_reference(ctx, inner);
    }
    let ty = ty.deref(ctx);
    !ty.is::<pliron::builtin::types::FP64Type>()
        && !ty.is::<pliron::builtin::types::IntegerType>()
        && !ty.is::<NumericType>()
}

/// Field names of a TypeScript tagged result, in WIT case order.
pub const RESULT_TAG: &str = "ok";
pub const RESULT_PAYLOADS: [&str; 2] = ["value", "error"];

/// A closed-shape record, referenced by value of this type.
///
/// Records are nominal and uniqued by name, so recursive shapes can declare an opaque record
/// first and supply its fields in a subsequent [RecordType::get] call.
/// Fields define physical storage order, while allocating operations preserve source evaluation order.
#[pliron_type(name = "perry.record")]
#[derive(Debug)]
pub struct RecordType {
    name: Identifier,
    fields: Option<Vec<(String, TypeHandle)>>,
    kind: RecordKind,
}

#[derive(Debug, thiserror::Error)]
enum RecordErr {
    #[error("unknown record kind {0}")]
    UnknownKind(String),
    #[error("record {0} already exists with different fields")]
    Mismatch(String),
    #[error("record {0} has duplicate field {1}")]
    DuplicateField(String, String),
    #[error("record {0} is used before its fields are known")]
    Opaque(String),
    #[error("a variant needs distinct literal tags")]
    InvalidVariant,
}

impl RecordType {
    /// Get or create the record `name`. Supplying `fields` defines an opaque record, and
    /// supplying different fields for an existing definition returns an error.
    pub fn get(
        ctx: &Context,
        name: Identifier,
        fields: Option<Vec<(String, TypeHandle)>>,
    ) -> Result<TypedHandle<Self>> {
        Self::get_kind(ctx, name, fields, RecordKind::Object)
    }

    /// Define or reserve a record, retaining its logical role across layout transformations.
    pub fn get_kind(
        ctx: &Context,
        name: Identifier,
        fields: Option<Vec<(String, TypeHandle)>>,
        kind: RecordKind,
    ) -> Result<TypedHandle<Self>> {
        if let Some(fields) = &fields {
            for (index, (field, _)) in fields.iter().enumerate() {
                if fields[..index].iter().any(|(other, _)| other == field) {
                    input_err_noloc!(RecordErr::DuplicateField(
                        name.to_string(),
                        field.to_string()
                    ))?
                }
            }
        }
        let handle = Type::instantiate(
            RecordType {
                name,
                fields: None,
                kind,
            },
            ctx,
        );
        if let Some(fields) = fields {
            // `TypedHandle` has no mutable access, so downcast the untyped handle back.
            let mut record = handle.to_handle().deref_mut(ctx);
            let record = record
                .downcast_mut::<RecordType>()
                .expect("the handle was instantiated from a RecordType just above");
            match &record.fields {
                Some(existing) if *existing != fields || record.kind != kind => {
                    input_err_noloc!(RecordErr::Mismatch(record.name.to_string()))?
                }
                Some(_) => {}
                None => {
                    record.fields = Some(fields);
                    record.kind = kind;
                }
            }
        }
        Ok(handle)
    }

    pub fn name(&self) -> &Identifier {
        &self.name
    }

    pub fn is_tuple(&self) -> bool {
        self.kind != RecordKind::Object
    }

    pub fn kind(&self) -> RecordKind {
        self.kind
    }

    pub fn fields(&self) -> Result<&[(String, TypeHandle)]> {
        match &self.fields {
            Some(fields) => Ok(fields),
            None => input_err_noloc!(RecordErr::Opaque(self.name.to_string())),
        }
    }

    /// Index and type of `field`, if the record is defined and has it.
    pub fn field(&self, field: &str) -> Option<(usize, TypeHandle)> {
        self.fields
            .as_ref()?
            .iter()
            .enumerate()
            .find(|(_, (name, _))| name == field)
            .map(|(index, (_, ty))| (index, *ty))
    }
}

impl PartialEq for RecordType {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
impl Eq for RecordType {}
impl std::hash::Hash for RecordType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl Verify for RecordType {
    fn verify(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }
}

dict_key!(RECORDS_IN_PRINTING, "perry_records_in_printing");

/// Returns true when `name` is already being printed further up (a recursive reference).
fn start_printing(state: &printable::State, name: &Identifier) -> bool {
    let mut aux = state.aux_data_mut();
    let active = aux
        .entry(RECORDS_IN_PRINTING.clone())
        .or_insert_with(|| Box::new(Vec::<Identifier>::new()))
        .downcast_mut::<Vec<Identifier>>()
        .expect("record printing state");
    if active.contains(name) {
        true
    } else {
        active.push(name.clone());
        false
    }
}

fn done_printing(state: &printable::State) {
    let mut aux = state.aux_data_mut();
    aux.get_mut(&RECORDS_IN_PRINTING)
        .and_then(|active| active.downcast_mut::<Vec<Identifier>>())
        .expect("record printing state")
        .pop();
}

impl Printable for RecordType {
    fn fmt(
        &self,
        ctx: &Context,
        state: &printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        if start_printing(state, &self.name) {
            return write!(f, "<{}>", self.name);
        }
        let result = (|| {
            write!(f, "<{}", self.name)?;
            if !matches!(self.kind, RecordKind::Object | RecordKind::Tuple) {
                write!(f, " {}", self.kind.name())?;
            }
            if let Some(fields) = &self.fields {
                write!(f, " {}", if self.is_tuple() { '[' } else { '{' })?;
                for (index, (name, ty)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ",")?;
                    }
                    if Identifier::is_valid(name) {
                        write!(f, " {name}")?;
                    } else {
                        write!(f, " {}", StringAttr::from(name.clone()).print(ctx, state))?;
                    }
                    write!(f, ": {}", ty.print(ctx, state))?;
                }
                write!(f, " {}", if self.is_tuple() { ']' } else { '}' })?;
            }
            write!(f, ">")
        })();
        done_printing(state);
        result
    }
}

impl Parsable for RecordType {
    type Arg = ();
    type Parsed = TypedHandle<Self>;

    fn parse<'a>(
        state_stream: &mut StateStream<'a>,
        _arg: Self::Arg,
    ) -> ParseResult<'a, Self::Parsed> {
        let field = || {
            spaced(
                Identifier::parser(())
                    .map(String::from)
                    .or(StringAttr::parser(()).map(String::from)),
            )
            .skip(spaced(token(':')))
            .and(spaced(type_parser()))
        };
        let fields = delimited_list_parser('{', '}', ',', field())
            .map(|fields| (fields, false))
            .or(delimited_list_parser('[', ']', ',', field()).map(|fields| (fields, true)));
        let mut parser = between(
            token('<'),
            token('>'),
            spaced((location(), Identifier::parser(())))
                .and(spaced(optional(Identifier::parser(()))))
                .and(spaced(optional(fields))),
        );
        let (((loc, name), kind), fields) = parser.parse_stream(state_stream).into_result()?.0;
        let (fields, tuple) = fields.map_or((None, false), |(fields, tuple)| (Some(fields), tuple));
        let kind = match kind {
            Some(kind) => match RecordKind::parse(kind.as_ref()) {
                Some(kind) => Ok(kind),
                None => input_err_noloc!(RecordErr::UnknownKind(kind.to_string())),
            },
            None => Ok(if tuple {
                RecordKind::Tuple
            } else {
                RecordKind::Object
            }),
        };
        kind.and_then(|kind| RecordType::get_kind(state_stream.state.ctx, name, fields, kind))
            .map_err(|mut error| {
                error.set_loc(loc);
                error
            })
            .into_parse_result()
    }
}

macro_rules! payload_type {
    ($(#[$doc:meta])* $ty:ident($name:literal, $accessor:ident)) => {
        $(#[$doc])*
        #[pliron_type(name = $name, generate_get = true, format = "`<` $payload `>`", verifier = "succ")]
        #[derive(Hash, PartialEq, Eq, Debug)]
        pub struct $ty {
            payload: TypeHandle,
        }

        impl $ty {
            pub fn $accessor(&self) -> TypeHandle {
                self.payload
            }
        }
    };
}

payload_type!(
    /// Storage for one mutable local prior to SSA construction by pliron `mem2reg`.
    SlotType("perry.slot", value)
);
payload_type!(
    /// A dense, growable array of a single element type.
    ArrayType("perry.array", element)
);
payload_type!(
    /// The eventual value of an async call, scoped to its export task.
    PromiseType("perry.promise", value)
);
payload_type!(
    /// The input promise of a `Promise.allSettled` result, read by `perry.settled_field`.
    SettledType("perry.settled", value)
);

/// A value with one or both absence states (`null`, `undefined`). Single-absence references
/// use a null pointer, and values admitting both states reserve a second unboxed sentinel.
#[pliron_type(
    name = "perry.optional",
    format = "`<` $value `,` $absence `>`",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct OptionalType {
    value: TypeHandle,
    absence: crate::attrs::Absence,
}

impl OptionalType {
    pub fn get(ctx: &Context, value: TypeHandle) -> TypedHandle<Self> {
        let existing = value
            .deref(ctx)
            .downcast_ref::<Self>()
            .map(|v| (v.value, v.absence));
        let (value, absence) = match existing {
            Some((inner, kind)) => (
                inner,
                if kind.admits_null() {
                    crate::attrs::Absence::Both
                } else {
                    kind
                },
            ),
            None => (value, crate::attrs::Absence::Undefined),
        };
        Self::get_with(ctx, value, absence)
    }

    pub fn get_with(
        ctx: &Context,
        value: TypeHandle,
        absence: crate::attrs::Absence,
    ) -> TypedHandle<Self> {
        Type::instantiate(Self { value, absence }, ctx)
    }

    pub fn value(&self) -> TypeHandle {
        self.value
    }

    pub fn absence(&self) -> crate::attrs::Absence {
        self.absence
    }
}

/// A finite union of JavaScript values, using variant storage without exposing tag/val properties.
#[pliron_type(
    name = "perry.value_union",
    generate_get = true,
    format = "`<` $variants `>`"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ValueUnionType {
    variants: TypeHandle,
}

impl ValueUnionType {
    pub fn variants(&self) -> TypeHandle {
        self.variants
    }

    /// `number | string`.
    pub fn number_string(ctx: &Context) -> TypedHandle<Self> {
        let cases = vec![
            (
                "number".into(),
                Some(pliron::builtin::types::FP64Type::get(ctx).into()),
            ),
            ("string".into(), Some(StringType::get(ctx).into())),
        ];
        let variants = VariantType::get(ctx, cases).into();
        Self::get(ctx, variants)
    }
}

impl Verify for ValueUnionType {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let ty = self.variants.deref(ctx);
        let Some(variants) = ty.downcast_ref::<VariantType>() else {
            return input_err_noloc!(RecordErr::InvalidVariant);
        };
        variants.verify(ctx)?;
        for (tag, payload) in variants.cases() {
            if payload.and_then(|ty| crate::variants::value_tag(ctx, ty)) != Some(tag.as_str()) {
                return input_err_noloc!(RecordErr::InvalidVariant);
            }
        }
        Ok(())
    }
}

/// A closed tagged union. Only its active payload owns storage.
#[pliron_type(name = "perry.variant")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct VariantType {
    cases: Vec<(String, Option<TypeHandle>)>,
}

impl VariantType {
    pub fn get(ctx: &Context, mut cases: Vec<(String, Option<TypeHandle>)>) -> TypedHandle<Self> {
        cases.sort_by(|a, b| a.0.cmp(&b.0));
        Type::instantiate(Self { cases }, ctx)
    }

    pub fn cases(&self) -> &[(String, Option<TypeHandle>)] {
        &self.cases
    }
}

impl Verify for VariantType {
    fn verify(&self, _: &Context) -> Result<()> {
        if self.cases.is_empty() || self.cases.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            input_err_noloc!(RecordErr::InvalidVariant)?;
        }
        Ok(())
    }
}

impl Printable for VariantType {
    fn fmt(
        &self,
        ctx: &Context,
        state: &printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        use pliron::builtin::attributes::StringAttr;
        write!(f, "<")?;
        for (index, (name, payload)) in self.cases.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", StringAttr::from(name.as_str()).print(ctx, state))?;
            if let Some(payload) = payload {
                write!(f, ": {}", payload.print(ctx, state))?;
            }
        }
        write!(f, ">")
    }
}

impl Parsable for VariantType {
    type Arg = ();
    type Parsed = TypedHandle<Self>;
    fn parse<'a>(stream: &mut StateStream<'a>, _: ()) -> ParseResult<'a, Self::Parsed> {
        use pliron::builtin::attributes::StringAttr;
        let case = spaced(StringAttr::parser(()))
            .and(optional(spaced(token(':')).with(spaced(type_parser()))));
        let cases = delimited_list_parser('<', '>', ',', case)
            .parse_stream(stream)
            .into_result()?
            .0;
        Ok(Self::get(
            stream.state.ctx,
            cases
                .into_iter()
                .map(|(name, ty)| (name.as_str().to_owned(), ty))
                .collect(),
        ))
        .into_parse_result()
    }
}

/// An opaque canonical resource identity. Ownership is checked by the shared descriptor.
#[pliron_type(
    name = "perry.resource",
    generate_get = true,
    format = "`<` $key `>`",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ResourceType {
    key: u32,
}
impl ResourceType {
    pub fn key(&self) -> u32 {
        self.key
    }
}
