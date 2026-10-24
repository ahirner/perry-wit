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

#[derive(Debug)]
enum RecordErr {
    UnknownKind(String),
    Mismatch(String),
    DuplicateField(String, String),
    Opaque(String),
    InvalidVariant,
}

impl core::fmt::Display for RecordErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownKind(kind) => write!(f, "unknown record kind {kind}"),
            Self::Mismatch(record) => {
                write!(f, "record {record} already exists with different fields")
            }
            Self::DuplicateField(record, field) => {
                write!(f, "record {record} has duplicate field {field}")
            }
            Self::Opaque(record) => {
                write!(f, "record {record} is used before its fields are known")
            }
            Self::InvalidVariant => f.write_str("a variant needs distinct literal tags"),
        }
    }
}

impl core::error::Error for RecordErr {}

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

/// Storage for one mutable local prior to SSA construction by pliron `mem2reg`.
#[pliron_type(
    name = "perry.slot",
    generate_get = true,
    format = "`<` $value `>`",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct SlotType {
    value: TypeHandle,
}

impl SlotType {
    pub fn value(&self) -> TypeHandle {
        self.value
    }
}

/// A dense, growable array of a single element type.
#[pliron_type(
    name = "perry.array",
    generate_get = true,
    format = "`<` $element `>`",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ArrayType {
    element: TypeHandle,
}

impl ArrayType {
    pub fn element(&self) -> TypeHandle {
        self.element
    }
}

/// The eventual value of an async call, scoped to its export task.
#[pliron_type(
    name = "perry.promise",
    generate_get = true,
    format = "`<` $value `>`",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct PromiseType {
    value: TypeHandle,
}

impl PromiseType {
    pub fn value(&self) -> TypeHandle {
        self.value
    }
}

/// An element of `Promise.allSettled`'s result: the settled input promise itself, read as
/// `{ status, value, reason }` (`perry.settled_field`).
#[pliron_type(
    name = "perry.settled",
    generate_get = true,
    format = "`<` $value `>`",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct SettledType {
    value: TypeHandle,
}

impl SettledType {
    pub fn value(&self) -> TypeHandle {
        self.value
    }
}

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
