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
use thiserror::Error;

macro_rules! record_kinds {
    ($($kind:ident => $name:literal),+ $(,)?) => {
        /// Logical record role, preserved when numeric storage is specialized.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Whether a value can retain native storage; unknown dialect types are conservative.
pub(crate) fn is_reference(ctx: &Context, ty: TypeHandle) -> bool {
    if let Some(inner) = crate::optional::optional_value(ctx, ty) {
        return is_reference(ctx, inner);
    }
    let ty = ty.deref(ctx);
    !ty.is::<pliron::builtin::types::FP64Type>()
        && !ty.is::<pliron::builtin::types::IntegerType>()
        && !ty.is::<NumericType>()
}

/// A numeric value retaining an exact signed/unsigned 64-bit integer or IEEE double.
/// Converting a narrower integer into this type explicitly widens its domain.
#[pliron_type(name = "perry.numeric", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct NumericType;

/// A closed-shape record, referenced by value of this type (subset rule R1).
///
/// Records are nominal and uniqued by name, so shapes may be recursive: create the record
/// without fields first and supply them on a later [RecordType::get].
/// Fields define storage order; source evaluation order is retained by the allocating operations.
#[pliron_type(name = "perry.record")]
#[derive(Debug)]
pub struct RecordType {
    name: Identifier,
    fields: Option<Vec<(String, TypeHandle)>>,
    kind: RecordKind,
}

#[derive(Debug, Error)]
enum RecordErr {
    #[error("unknown record kind {0}")]
    UnknownKind(String),
    #[error("record {0} already exists with different fields")]
    Mismatch(String),
    #[error("record {0} has duplicate field {1}")]
    DuplicateField(String, String),
    #[error("record {0} is used before its fields are known")]
    Opaque(String),
}

impl RecordType {
    /// Get or create the record `name`. Supplying `fields` defines an opaque record;
    /// supplying different fields for an existing definition is an error.
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

/// Storage for one mutable local before SSA construction; removed by pliron `mem2reg`.
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

/// A dense, growable array of one element type (subset: no holes, no mixed elements).
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

/// An immutable, validated UTF-8 string indexed by Unicode scalar values.
#[pliron_type(name = "perry.string", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct StringType;

/// Immutable sign and 64-bit magnitude, preserving every WIT s64/u64 value.
#[pliron_type(name = "perry.bigint", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct BigIntType;

/// A `Uint8Array`: a view `{ length, data }` over bytes in the task region; `subarray`
/// views share their bytes.
#[pliron_type(name = "perry.bytes", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct BytesType;

/// Read-only outbound WIT byte storage, accepting either UTF-8 strings or mutable bytes.
#[pliron_type(
    name = "perry.byte_input",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ByteInputType;

#[pliron_type(name = "perry.url", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct UrlType;

#[pliron_type(
    name = "perry.search_params",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct SearchParamsType;

/// An internal cursor over one fully validated immutable JSON document.
#[pliron_type(
    name = "perry.json_cursor",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct JsonCursorType;

/// A parsed JSON value (`JSON.parse`): a node of the JSON helper's graph, or `undefined`.
/// Untyped data is a JSON tree, never `any` (subset rule R6).
#[pliron_type(name = "perry.json", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct JsonType;

/// A mutable Date object containing epoch milliseconds or NaN.
#[pliron_type(name = "perry.date", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct DateType;

/// `fs.Stats` of `stat()`: the entry type and size, read through `perry.intrinsic`.
#[pliron_type(name = "perry.stats", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct StatsType;

/// A Fetch `Headers` list.
#[pliron_type(name = "perry.headers", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct HeadersType;

/// A Fetch `Response`.
#[pliron_type(
    name = "perry.response",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ResponseType;

/// A Fetch `Request`.
#[pliron_type(name = "perry.request", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct RequestType;

/// A Web `WritableStream` over a console output (`Writable.toWeb(process.stdout)`).
#[pliron_type(
    name = "perry.writable",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct WritableType;

/// A `WritableStreamDefaultWriter`.
#[pliron_type(name = "perry.writer", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct WriterType;

/// A `ReadableStream<Uint8Array>` over a WIT `stream<u8>`.
#[pliron_type(
    name = "perry.readable",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ReadableType;

/// A detachable `ArrayBuffer` backing mutable byte views.
#[pliron_type(
    name = "perry.byte_buffer",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ByteBufferType;

/// A `ReadableStreamBYOBReader`.
#[pliron_type(
    name = "perry.byob_reader",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ByobReaderType;

/// A `ReadableStreamDefaultReader`.
#[pliron_type(name = "perry.reader", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ReaderType;

/// A reader's `{ done, value }`.
#[pliron_type(
    name = "perry.read_result",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ReadResultType;

/// A `Temporal.Instant`: 16 opaque bytes, little-endian `i128` epoch nanoseconds
/// (the `time-helper` codec).
#[pliron_type(name = "perry.instant", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct InstantType;

/// A `Temporal.PlainDate`: 16 opaque bytes, an `i32` epoch day and zero nanoseconds.
#[pliron_type(
    name = "perry.plain_date",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct PlainDateType;

/// A `Temporal.PlainDateTime`: 16 opaque bytes, an `i32` epoch day and the `u64`
/// nanosecond of that day.
#[pliron_type(
    name = "perry.plain_date_time",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct PlainDateTimeType;

/// An `AbortController` and its `AbortSignal` (one object): aborting rejects the timers that
/// watch it.
#[pliron_type(name = "perry.abort", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct AbortType;

/// An HTTP request a handler serves (`wasi:http/handler`): its parts, its body and the
/// response being built.
#[pliron_type(
    name = "perry.incoming",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct IncomingType;

/// A caught exception (`catch (e)`): an `Error` object or a boxed thrown value. Lives in the
/// task like every other value (R3, R4).
#[pliron_type(
    name = "perry.exception",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ExceptionType;

/// The eventual value of an async call; lives no longer than its export task (subset rule R3).
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

/// A value with one or both absence states. Single-absence references use a null pointer;
/// values admitting both states reserve a second sentinel without boxing the payload.
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

/// Stateful UTF-8 decoder with at most three pending bytes.
#[pliron_type(name = "perry.decoder", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct DecoderType;

/// Immutable, statically compiled regex program; no mutable lastIndex state.
#[pliron_type(name = "perry.regex", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct RegexType;

/// Canonical result area and argument owners of a suspended host call.
#[pliron_type(
    name = "perry.transfer",
    generate_get = true,
    format = "",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct TransferType;

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
            input_err_noloc!(InvalidVariant)?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
#[error("a variant needs distinct literal tags")]
struct InvalidVariant;

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

/// The finite primitive union `number | string`, with an exact owner slot for its string case.
#[pliron_type(
    name = "perry.number_string",
    generate_get = true,
    format,
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct NumberStringType;

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
