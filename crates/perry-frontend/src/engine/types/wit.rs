//! Source-level inference types of resolved WIT values.
use super::*;
use crate::wit::WitValue;
use perry_contract::{
    names::NamedType,
    types::{Contract, Input, convert},
};
use wit_parser::Type;

impl TypeTable {
    pub(crate) fn export_type(&mut self, ctx: &Context, value: WitValue<'_>) -> Ty {
        match value.integer() {
            Some((64, _)) => Ty::BIGINT,
            Some(_) => Ty::NUMBER,
            None => self.wit_type(ctx, value, false),
        }
    }

    pub(crate) fn wit_type(&mut self, ctx: &Context, value: WitValue<'_>, expand: bool) -> Ty {
        let input = match value.ty {
            Type::Id(id) if expand => Input::Definition(id),
            ty => Input::Reference(ty),
        };
        let named = |table: &mut Self, id| {
            let name = NamedType::get(value.resolve, id)?.qualified();
            Some(table.named(ctx, &name))
        };
        let object = |table: &mut Self, fields: Vec<(String, Ty)>| {
            let props: Vec<_> = fields
                .into_iter()
                .map(|(name, ty)| ObjectProp {
                    name: table.intern_name(&name),
                    ty,
                    optional: false,
                    readonly: false,
                })
                .collect();
            table.object(&props, None)
        };
        convert(
            value.resolve,
            input,
            self,
            &named,
            &|table, contract| match contract {
                Contract::Scalar(ty) => match ty {
                    Type::Bool => Ty::BOOLEAN,
                    Type::String | Type::Char => Ty::STRING,
                    Type::F64 | Type::F32 => Ty::NUMBER,
                    ty => (WitValue { ty, ..value })
                        .integer()
                        .map_or(Ty::OPEN, |(width, sign)| {
                            table.intern_kind(TyKind::Integer(IntegerType::get(ctx, width, sign)))
                        }),
                },
                Contract::Resource { id, .. } => named(
                    table,
                    perry_contract::names::canonical_type(value.resolve, id),
                )
                .unwrap_or(Ty::OPEN),
                Contract::Bytes => table.named(ctx, "Uint8Array"),
                Contract::ByteStream => {
                    let bytes = table.named(ctx, "Uint8Array");
                    table.generic(
                        shapes::TypeName::new(ctx, "ReadableStream".into()),
                        &[bytes],
                    )
                }
                Contract::List(item) => table.array(item),
                Contract::Option(item) => table.with_absences(item, crate::Absence::Null),
                Contract::Tuple(items) => table.tuple(&items),
                Contract::Record(fields) => object(table, fields),
                Contract::Enum(tags) => {
                    let tags: Vec<_> = tags.map(|tag| table.literal(ctx, tag)).collect();
                    table.union(&tags)
                }
                Contract::Variant(cases) => {
                    let cases: Vec<_> = cases
                        .into_iter()
                        .map(|(tag, value)| {
                            let tag = table.literal(ctx, tag);
                            object(
                                table,
                                std::iter::once(("tag".into(), tag))
                                    .chain(value.map(|value| ("val".into(), value)))
                                    .collect(),
                            )
                        })
                        .collect();
                    table.union(&cases)
                }
                Contract::Result { ok, err } => {
                    let cases = [("value", ok), ("error", err)].map(|(name, value)| {
                        object(
                            table,
                            std::iter::once(("ok".into(), Ty::BOOLEAN))
                                .chain(value.map(|value| (name.into(), value)))
                                .collect(),
                        )
                    });
                    table.union(&cases)
                }
                Contract::Unsupported(_) => Ty::OPEN,
            },
        )
    }
}
