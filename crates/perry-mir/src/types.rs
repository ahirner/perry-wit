//! Types of the `perry` dialect.
use pliron::{
    combine::{Parser, between, optional, token},
    common_traits::Verify,
    context::Context,
    derive::pliron_type,
    dict_key,
    identifier::Identifier,
    input_err_noloc,
    irfmt::parsers::{delimited_list_parser, location, spaced, type_parser},
    location::Located,
    parsable::{IntoParseResult, Parsable, ParseResult, StateStream},
    printable::{self, Printable},
    result::Result,
    r#type::{Type, TypeHandle, TypedHandle},
};
use std::any::TypeId;
use thiserror::Error;

/// A thrown value: an Error or a primitive, kept distinct from general dynamic values.
#[pliron_type(
    name = "perry.exception",
    generate_get = true,
    format = "",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct ExceptionType;

/// An explicit JSON variant tree, distinct from arbitrary dynamic values.
#[pliron_type(
    name = "perry.json",
    generate_get = true,
    format = "",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct JsonType;

/// A task-owned post-order traversal paused around synchronous JSON reviver calls.
#[pliron_type(
    name = "perry.json_reviver",
    generate_get = true,
    format = "",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct JsonReviverType;

/// A fixed-size ArrayBuffer owned by a memory tier.
#[pliron_type(
    name = "perry.buffer",
    generate_get = true,
    format = "",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct BufferType;

/// A Uint8Array view sharing the storage of an ArrayBuffer.
#[pliron_type(
    name = "perry.bytes",
    generate_get = true,
    format = "",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct BytesType;

/// A closed-shape record, referenced by value of this type (subset rule R1).
///
/// Records are nominal and uniqued by name, so shapes may be recursive: create the record
/// without fields first and supply them on a later [RecordType::get].
/// The builder canonicalizes fields into sorted layout order. Each allocation separately
/// records property creation order, so structurally equal records retain enumeration order.
#[pliron_type(name = "perry.record")]
#[derive(Debug)]
pub struct RecordType {
    name: Identifier,
    fields: Option<Vec<(Identifier, TypeHandle)>>,
}

#[derive(Debug, Error)]
pub enum RecordErr {
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
        fields: Option<Vec<(Identifier, TypeHandle)>>,
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
                name: name.clone(),
                fields: None,
            },
            ctx,
        );
        if let Some(fields) = fields {
            let mut record = handle.to_handle().deref_mut(ctx);
            let record = record.downcast_mut::<RecordType>().unwrap();
            match &record.fields {
                Some(existing) if *existing != fields => {
                    input_err_noloc!(RecordErr::Mismatch(name.to_string()))?
                }
                Some(_) => {}
                None => record.fields = Some(fields),
            }
        }
        Ok(handle)
    }

    pub fn name(&self) -> &Identifier {
        &self.name
    }

    pub fn fields(&self) -> Result<&[(Identifier, TypeHandle)]> {
        match &self.fields {
            Some(fields) => Ok(fields),
            None => input_err_noloc!(RecordErr::Opaque(self.name.to_string())),
        }
    }

    /// Index and type of `field`, if the record is defined and has it.
    pub fn field(&self, field: &Identifier) -> Option<(usize, TypeHandle)> {
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

dict_key!(NAMED_TYPES_IN_PRINTING, "perry_named_types_in_printing");

/// Returns true when this type and name are already being printed further up (recursion).
fn start_printing<T: 'static>(state: &printable::State, name: &Identifier) -> bool {
    let mut aux = state.aux_data_mut();
    let active = aux
        .entry(NAMED_TYPES_IN_PRINTING.clone())
        .or_insert_with(|| Box::new(Vec::<(TypeId, Identifier)>::new()))
        .downcast_mut::<Vec<(TypeId, Identifier)>>()
        .expect("named type printing state");
    let identity = (TypeId::of::<T>(), name.clone());
    if active.contains(&identity) {
        true
    } else {
        active.push(identity);
        false
    }
}

fn done_printing(state: &printable::State) {
    let mut aux = state.aux_data_mut();
    aux.get_mut(&NAMED_TYPES_IN_PRINTING)
        .and_then(|active| active.downcast_mut::<Vec<(TypeId, Identifier)>>())
        .expect("named type printing state")
        .pop();
}

impl Printable for RecordType {
    fn fmt(
        &self,
        ctx: &Context,
        state: &printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        if start_printing::<Self>(state, &self.name) {
            return write!(f, "<{}>", self.name);
        }
        let result = (|| {
            write!(f, "<{}", self.name)?;
            if let Some(fields) = &self.fields {
                write!(f, " {{")?;
                for (index, (name, ty)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, " {name}: {}", ty.print(ctx, state))?;
                }
                write!(f, " }}")?;
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
        let field = spaced(Identifier::parser(()))
            .skip(spaced(token(':')))
            .and(spaced(type_parser()));
        let fields = delimited_list_parser('{', '}', ',', field);
        let mut parser = between(
            token('<'),
            token('>'),
            spaced((location(), Identifier::parser(()))).and(spaced(optional(fields))),
        );
        let ((loc, name), fields) = parser.parse_stream(state_stream).into_result()?.0;
        RecordType::get(state_stream.state.ctx, name, fields)
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

/// An ordered, mutable string-keyed dictionary with one statically known value type (R1).
/// Named declarations reserve identity before resolving a recursive element type.
#[pliron_type(name = "perry.dictionary")]
#[derive(Debug)]
pub struct DictionaryType {
    identity: DictionaryIdentity,
    element: Option<TypeHandle>,
}

#[derive(Debug, Hash, PartialEq, Eq)]
enum DictionaryIdentity {
    Structural(TypeHandle),
    Named(Identifier),
}

impl DictionaryType {
    pub fn get(ctx: &Context, element: TypeHandle) -> TypedHandle<Self> {
        Type::instantiate(
            Self {
                identity: DictionaryIdentity::Structural(element),
                element: Some(element),
            },
            ctx,
        )
    }
    pub fn named(
        ctx: &Context,
        name: Identifier,
        element: Option<TypeHandle>,
    ) -> Result<TypedHandle<Self>> {
        let handle = Type::instantiate(
            Self {
                identity: DictionaryIdentity::Named(name.clone()),
                element: None,
            },
            ctx,
        );
        if let Some(element) = element {
            let mut ty = handle.to_handle().deref_mut(ctx);
            let dictionary = ty.downcast_mut::<Self>().unwrap();
            if dictionary
                .element
                .is_some_and(|existing| existing != element)
            {
                input_err_noloc!(DictionaryMismatch(name.to_string()))?;
            }
            dictionary.element = Some(element);
        }
        Ok(handle)
    }
    pub fn element(&self) -> Option<TypeHandle> {
        self.element
    }
}
#[derive(Debug, Error)]
#[error("dictionary {0} already exists with another element type")]
struct DictionaryMismatch(String);
impl PartialEq for DictionaryType {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}
impl Eq for DictionaryType {}
impl std::hash::Hash for DictionaryType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity.hash(state);
    }
}
impl Verify for DictionaryType {
    fn verify(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }
}
impl Printable for DictionaryType {
    fn fmt(
        &self,
        ctx: &Context,
        state: &printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        let DictionaryIdentity::Named(name) = &self.identity else {
            return write!(
                f,
                "<{}>",
                self.element
                    .expect("structural dictionary element")
                    .print(ctx, state)
            );
        };
        if start_printing::<Self>(state, name) {
            return write!(f, "<@{name}>");
        }
        let result = (|| {
            write!(f, "<@{name}")?;
            if let Some(element) = self.element {
                write!(f, ": {}", element.print(ctx, state))?;
            }
            write!(f, ">")
        })();
        done_printing(state);
        result
    }
}
impl Parsable for DictionaryType {
    type Arg = ();
    type Parsed = TypedHandle<Self>;
    fn parse<'a>(stream: &mut StateStream<'a>, _arg: ()) -> ParseResult<'a, Self::Parsed> {
        enum Declaration {
            Named(Identifier, Option<TypeHandle>),
            Structural(TypeHandle),
        }
        let named = token('@')
            .with(Identifier::parser(()))
            .and(optional(spaced(token(':')).with(spaced(type_parser()))))
            .map(|(name, element)| Declaration::Named(name, element));
        let mut parser = between(
            token('<'),
            token('>'),
            spaced(named.or(type_parser().map(Declaration::Structural))),
        );
        let declaration = parser.parse_stream(stream).into_result()?.0;
        match declaration {
            Declaration::Named(name, element) => Self::named(stream.state.ctx, name, element),
            Declaration::Structural(element) => Ok(Self::get(stream.state.ctx, element)),
        }
        .into_parse_result()
    }
}

/// An immutable, validated UTF-8 string indexed by Unicode scalar values.
#[pliron_type(name = "perry.string", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct StringType;

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

/// `T | undefined`. Produced by reads that may miss (out-of-bounds array reads, later optional
/// fields); consumed through `perry.to_number`, `perry.to_string`, `perry.is_undefined` and
/// `perry.unwrap`. The representation is chosen at lowering: a reserved NaN payload for numbers
/// (so arithmetic yields NaN as in ECMAScript), a null reference for references.
#[pliron_type(
    name = "perry.optional",
    generate_get = true,
    format = "`<` $value `>`",
    verifier = "succ"
)]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct OptionalType {
    value: TypeHandle,
}

impl OptionalType {
    pub fn value(&self) -> TypeHandle {
        self.value
    }
}
