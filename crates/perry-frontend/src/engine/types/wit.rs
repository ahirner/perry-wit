//! Source-level inference types of resolved WIT values.
use super::*;
use crate::wit::{Foreign, WitValue, WorldTypes};
use perry_contract::{
    names::NamedType,
    types::{Contract, Input, convert},
};
use wit_parser::Type;

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) enum WitView {
    Reference,
    Definition,
    Boundary,
}

impl TypeTable {
    pub(crate) fn export_type(&mut self, ctx: &Context, value: WitValue<'_>) -> Ty {
        match value.integer() {
            Some((64, _)) => Ty::BIGINT,
            Some(_) => Ty::NUMBER,
            None => self.wit_type(ctx, value, WitView::Reference),
        }
    }

    pub(crate) fn wit_type(&mut self, ctx: &Context, value: WitValue<'_>, view: WitView) -> Ty {
        let key = (std::ptr::from_ref(value.resolve), value.ty, view);
        if let Some(&ty) = self.wit_shapes.get(&key) {
            return ty;
        }
        let input = match value.ty {
            Type::Id(id) if view != WitView::Reference => Input::Definition(id),
            ty => Input::Reference(ty),
        };
        let named = |table: &mut Self, id| {
            let name = NamedType::get(value.resolve, id)?.qualified();
            Some(table.named(ctx, &name))
        };
        let case = |table: &mut Self, name, ty, payload, value: Option<Ty>| {
            let tag = table.property(name, ty, false);
            match value {
                None => table.object(&[tag], None),
                Some(ty) => {
                    let val = table.property(payload, ty, false);
                    table.object(&[tag, val], None)
                }
            }
        };
        let ty = convert(
            value.resolve,
            input,
            self,
            &|table, id| {
                if view == WitView::Boundary {
                    Some(table.wit_type(
                        ctx,
                        WitValue {
                            ty: Type::Id(id),
                            ..value
                        },
                        view,
                    ))
                } else {
                    named(table, id)
                }
            },
            &|table, contract| match contract {
                Contract::Alias(ty) => ty,
                Contract::Scalar(scalar) => {
                    let ty = match scalar {
                        Type::Bool => Ty::BOOLEAN,
                        Type::String | Type::Char => Ty::STRING,
                        Type::F64 | Type::F32 => Ty::NUMBER,
                        ty => (WitValue { ty, ..value }).integer().map_or(
                            Ty::OPEN,
                            |(width, sign)| {
                                table.intern_kind(TyKind::Integer(IntegerType::get(
                                    ctx, width, sign,
                                )))
                            },
                        ),
                    };
                    table.wit_shapes.insert((value.resolve, scalar, view), ty);
                    ty
                }
                Contract::Resource { id, .. } => {
                    let id = perry_contract::names::canonical_type(value.resolve, id);
                    if view == WitView::Boundary {
                        table.intern_kind(TyKind::WitCarrier(
                            perry_mir::ResourceType::get(ctx, id.index() as u32).into(),
                        ))
                    } else {
                        named(table, id).unwrap_or(Ty::OPEN)
                    }
                }
                Contract::Bytes if view == WitView::Boundary => {
                    table.intern_kind(TyKind::WitCarrier(perry_mir::BytesType::get(ctx).into()))
                }
                Contract::ByteStream if view == WitView::Boundary => {
                    table.intern_kind(TyKind::WitCarrier(perry_mir::ReadableType::get(ctx).into()))
                }
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
                Contract::Record(fields) => {
                    let props = fields
                        .iter()
                        .map(|(name, ty)| table.property(name, *ty, false))
                        .collect::<Vec<_>>();
                    table.object(&props, None)
                }
                Contract::Enum(tags) => {
                    let tags: Vec<_> = tags
                        .cases
                        .iter()
                        .map(|case| table.literal(ctx, &case.name))
                        .collect();
                    table.union(&tags)
                }
                Contract::Variant(_, cases) => {
                    let cases: Vec<_> = cases
                        .into_iter()
                        .map(|(tag, value)| {
                            let tag = table.literal(ctx, tag);
                            case(table, "tag", tag, "val", value)
                        })
                        .collect();
                    if view == WitView::Boundary {
                        let arms = table.ty_pool.intern(&mut table.members_index, &cases);
                        table.intern_kind(TyKind::Variant(arms))
                    } else {
                        table.union(&cases)
                    }
                }
                Contract::Result { ok, err } => {
                    use perry_mir::{RESULT_PAYLOADS, RESULT_TAG};
                    let [value, error] = RESULT_PAYLOADS;
                    let cases = [(value, ok), (error, err)]
                        .map(|(name, payload)| case(table, RESULT_TAG, Ty::BOOLEAN, name, payload));
                    table.union(&cases)
                }
                Contract::Unsupported(_) => Ty::OPEN,
            },
        );
        self.wit_shapes.insert(key, ty);
        ty
    }

    pub(crate) fn intern_imports(&mut self, ctx: &Context, world: &WorldTypes<'_>) {
        let imports: std::collections::BTreeMap<_, _> = world
            .foreign()
            .iter()
            .filter_map(|(name, f)| match f {
                Foreign::Function(signature) => Some((name, signature)),
                _ => None,
            })
            .collect();
        for signature in imports.into_values() {
            for &value in signature.params.iter().chain(&signature.result) {
                self.wit_type(ctx, value, WitView::Boundary);
            }
        }
    }

    pub(crate) fn boundary_type(&self, value: WitValue<'_>) -> perry_wit_diagnostics::Result<Ty> {
        self.wit_shapes
            .get(&(value.resolve, value.ty, WitView::Boundary))
            .copied()
            .ok_or_else(|| perry_wit_diagnostics::internal!("missing WIT import type"))
    }
}
