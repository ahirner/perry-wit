//! Source-level inference types of resolved WIT values.
use super::*;
use crate::wit::{WitValue, WorldTypes};
use perry_contract::{
    names::NamedType,
    types::{Contract, Scalar, project},
};
use wit_parser::Type;

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) enum WitView {
    Reference,
    Definition,
    Boundary,
    Input,
}

impl TypeTable {
    pub(crate) fn result_record(&mut self, payloads: [Option<Ty>; 2]) -> Ty {
        let mut fields = vec![self.property(perry_mir::RESULT_TAG, Ty::BOOLEAN, false)];
        for (name, payload) in perry_mir::RESULT_PAYLOADS.into_iter().zip(payloads) {
            if let Some(ty) = payload {
                fields.push(self.property(name, ty, true));
            }
        }
        self.object(&fields, None)
    }

    pub(crate) fn wit_input(&mut self, ctx: &Context, ty: Ty) -> Ty {
        let (present, absent) = self.strip_nullish(ty);
        match self.kind(present) {
            TyKind::Named(_)
            | TyKind::Generic { .. }
            | TyKind::Object { .. }
            | TyKind::Array(_)
            | TyKind::Tuple(_)
            | TyKind::Variant(_)
            | TyKind::Union(_)
            | TyKind::WitCarrier(_)
            | TyKind::ClassInst(_)
            | TyKind::Cell { .. } => {
                let input = self.generic(shapes::TypeName::new(ctx, "WitInput".into()), &[present]);
                self.with_absences(input, absent)
            }
            _ => ty,
        }
    }

    pub(crate) fn export_type(&mut self, ctx: &Context, value: WitValue<'_>) -> Ty {
        match value.integer() {
            Some(domain) if domain.bits() == 64 => Ty::BIGINT,
            Some(_) => Ty::NUMBER,
            None => self.wit_type(ctx, value, WitView::Reference),
        }
    }

    pub(crate) fn wit_type(&mut self, ctx: &Context, value: WitValue<'_>, view: WitView) -> Ty {
        let key = (value.ty, view);
        if let Some(&ty) = self.wit_shapes.get(&key) {
            return ty;
        }
        if view == WitView::Input {
            let boundary = self.wit_type(ctx, value, WitView::Boundary);
            let ty = self.wit_input(ctx, boundary);
            self.wit_shapes.insert(key, ty);
            return ty;
        }
        let named = |table: &mut Self, id| {
            let name = NamedType::get(value.resolve, id)?.qualified();
            Some(table.named(ctx, &name))
        };
        if view == WitView::Reference
            && let Type::Id(id) = value.ty
            && let Some(ty) = named(self, id)
        {
            self.wit_shapes.insert(key, ty);
            return ty;
        }
        let child_view = if view == WitView::Definition {
            WitView::Reference
        } else {
            view
        };
        let contract = project(value.resolve, value.ty, |&ty| {
            self.wit_type(ctx, WitValue { ty, ..value }, child_view)
        });
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
        let ty = match contract {
            Contract::Alias(ty) => ty,
            Contract::Scalar(Scalar::Bool) => Ty::BOOLEAN,
            Contract::Scalar(Scalar::F32 | Scalar::F64) => Ty::NUMBER,
            Contract::Scalar(Scalar::Integer(domain)) => {
                self.intern_kind(TyKind::Integer(perry_mir::integer_type(ctx, domain)))
            }
            Contract::String | Contract::Char => Ty::STRING,
            Contract::ErrorContext => Ty::OPEN,
            Contract::Resource { id, .. } => {
                let id = perry_contract::names::canonical_type(value.resolve, id);
                if view == WitView::Boundary {
                    self.intern_kind(TyKind::WitCarrier(
                        perry_mir::ResourceType::get(ctx, id.index() as u32).into(),
                    ))
                } else {
                    named(self, id).unwrap_or(Ty::OPEN)
                }
            }
            Contract::Bytes if view == WitView::Boundary => {
                self.intern_kind(TyKind::WitCarrier(perry_mir::BytesType::get(ctx).into()))
            }
            Contract::ByteStream if view == WitView::Boundary => {
                self.intern_kind(TyKind::WitCarrier(perry_mir::ReadableType::get(ctx).into()))
            }
            Contract::Bytes => self.named(ctx, "Uint8Array"),
            Contract::ByteStream => {
                let bytes = self.named(ctx, "Uint8Array");
                self.generic(
                    shapes::TypeName::new(ctx, "ReadableStream".into()),
                    &[bytes],
                )
            }
            Contract::List(item) => self.array(item),
            Contract::Option(item) => self.with_absences(item, crate::Absence::Null),
            Contract::Tuple(items) => self.tuple(&items),
            Contract::Record(fields) => {
                let props = fields
                    .iter()
                    .map(|(name, ty)| self.property(name, *ty, false))
                    .collect::<Vec<_>>();
                self.object(&props, None)
            }
            Contract::Enum(tags) => {
                let tags: Vec<_> = tags
                    .cases
                    .iter()
                    .map(|case| self.literal(ctx, &case.name))
                    .collect();
                self.union(&tags)
            }
            Contract::Variant(_, cases) if view == WitView::Boundary => {
                let cases: Vec<_> = cases
                    .into_iter()
                    .map(|(tag, payload)| (shapes::TypeName::new(ctx, tag.into()), payload))
                    .collect();
                let cases = CasesId::intern(&mut self.cases, &cases);
                self.intern_kind(TyKind::Variant(cases))
            }
            Contract::Variant(_, cases) => {
                let cases: Vec<_> = cases
                    .into_iter()
                    .map(|(tag, value)| {
                        let tag = self.literal(ctx, tag);
                        case(self, "tag", tag, "val", value)
                    })
                    .collect();
                self.union(&cases)
            }
            Contract::Result { ok, err } if view == WitView::Boundary => {
                self.result_record([ok, err])
            }
            Contract::Result { ok, err } => {
                use perry_mir::{RESULT_PAYLOADS, RESULT_TAG};
                let [value, error] = RESULT_PAYLOADS;
                let cases = [(value, ok), (error, err)]
                    .map(|(name, payload)| case(self, RESULT_TAG, Ty::BOOLEAN, name, payload));
                self.union(&cases)
            }
            Contract::Unsupported(_) => Ty::OPEN,
        };
        self.wit_shapes.insert(key, ty);
        ty
    }

    pub(crate) fn intern_imports(&mut self, ctx: &Context, world: &WorldTypes<'_>) {
        for signature in world.signatures() {
            let Some(params) = &signature.abi_params else {
                continue;
            };
            for &value in params {
                self.wit_type(ctx, value, WitView::Input);
            }
            if let Some(value) = signature.result {
                self.wit_type(ctx, value, WitView::Boundary);
            }
        }
    }

    pub(crate) fn boundary_type(
        &self,
        value: WitValue<'_>,
        view: WitView,
    ) -> perry_wit_diagnostics::Result<Ty> {
        self.wit_shapes
            .get(&(value.ty, view))
            .copied()
            .ok_or_else(|| perry_wit_diagnostics::internal!("missing WIT import type"))
    }
}
