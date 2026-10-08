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
use thiserror::Error;

/// A closed-shape record, referenced by value of this type (subset rule R1).
///
/// Records are nominal and uniqued by name, so shapes may be recursive: create the record
/// without fields first and supply them on a later [RecordType::get].
/// Field order is the source declaration order, which fixes evaluation and enumeration order;
/// physical layout is decided by a later pass (P3.6), not by this type.
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
