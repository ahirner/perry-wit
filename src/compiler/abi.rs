//! Canonical wasm32 layouts at the WIT binding boundary.
use perry_codegen::boundary::{AbiField, AbiKind, AbiType, SumLayout};
use perry_contract::{
    names::canonical_type,
    types::{Contract, project},
};
use perry_wit_diagnostics::{Diagnostic, Result, diag};
use std::{collections::HashMap, sync::Arc};
use wit_parser::{
    Function, Int, LiftLowerAbi, ManglingAndAbi, Resolve, SizeAlign, Type,
    abi::{FlatTypes, WasmSignature, WasmType},
};

pub(super) struct Layouts<'r> {
    pub(super) resolve: &'r Resolve,
    sizes: SizeAlign,
    types: HashMap<Type, Arc<AbiType>>,
}

impl<'r> Layouts<'r> {
    pub(super) fn new(resolve: &'r Resolve) -> Result<Self> {
        let mut sizes = SizeAlign::default();
        sizes
            .fill(resolve)
            .map_err(|e| Diagnostic::internal(e.to_string()))?;
        Ok(Self {
            resolve,
            sizes,
            types: HashMap::new(),
        })
    }
    fn fields<'a>(&mut self, types: impl Iterator<Item = &'a Type>) -> Result<Vec<AbiField>> {
        self.sizes
            .field_offsets(types)
            .into_iter()
            .map(|(offset, ty)| self.field(*ty, offset.size_wasm32() as u32))
            .collect()
    }

    fn payloads<'a>(
        &mut self,
        tag: Int,
        cases: impl IntoIterator<Item = Option<&'a Type>>,
    ) -> impl FnMut(Option<Type>) -> Result<Option<AbiField>> + '_ {
        let offset = self.sizes.payload_offset(tag, cases).size_wasm32() as u32;
        move |ty| ty.map(|ty| self.field(ty, offset)).transpose()
    }

    pub(super) fn signature(&self, function: &Function, imported: bool) -> WasmSignature {
        let abi = ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback).for_func(function);
        let variant = if imported {
            abi.import_variant()
        } else {
            abi.export_variant()
        };
        self.resolve.wasm_signature(variant, function)
    }

    pub(super) fn parameters(
        &mut self,
        function: &Function,
        core: &WasmSignature,
    ) -> Result<(Vec<AbiField>, Option<u32>)> {
        let types = || function.params.iter().map(|p| &p.ty);
        let params = self.fields(types())?;
        let area = core
            .indirect_params
            .then(|| self.sizes.record(types()).size.size_wasm32() as u32);
        Ok((params, area))
    }
    pub(super) fn field(&mut self, ty: Type, offset: u32) -> Result<AbiField> {
        Ok(AbiField {
            ty: self.value(ty)?,
            offset,
        })
    }

    pub(super) fn value(&mut self, ty: Type) -> Result<Arc<AbiType>> {
        if let Some(layout) = self.types.get(&ty) {
            return Ok(Arc::clone(layout));
        }
        let layout = self.define(ty)?;
        self.types.insert(ty, Arc::clone(&layout));
        Ok(layout)
    }

    fn define(&mut self, mut ty: Type) -> Result<Arc<AbiType>> {
        let kind = match project(self.resolve, ty, |ty| *ty) {
            Contract::Alias(inner) => return self.value(inner),
            Contract::Char => return Err(unsupported("char")),
            Contract::ErrorContext => return Err(unsupported("error-context")),
            Contract::String => AbiKind::String,
            Contract::Scalar(scalar) => AbiKind::Scalar(scalar),
            Contract::Resource { id, owned } => {
                ty = Type::U32;
                AbiKind::Resource {
                    key: canonical_type(self.resolve, id).index() as u32,
                    owned,
                }
            }
            Contract::ByteStream => AbiKind::ByteStream,
            Contract::Bytes => AbiKind::List(self.value(Type::U8)?),
            Contract::List(item) => AbiKind::List(self.value(item)?),
            Contract::Tuple(types) => {
                let names = (0..types.len()).map(|i| format!("_{i}"));
                AbiKind::record(ty, names.zip(self.fields(types.iter())?))
            }
            Contract::Record(fields) => {
                let layouts = self.fields(fields.iter().map(|(_, ty)| ty))?;
                let names = fields.into_iter().map(|(name, _)| name);
                AbiKind::record(ty, names.zip(layouts))
            }
            Contract::Option(item) => {
                let offset = self
                    .sizes
                    .payload_offset(Int::U8, [Some(&item)])
                    .size_wasm32() as u32;
                AbiKind::Sum(SumLayout::Option(self.field(item, offset)?))
            }
            Contract::Result { ok, err } => {
                let mut payload = self.payloads(Int::U8, [ok.as_ref(), err.as_ref()]);
                AbiKind::Sum(SumLayout::Result([payload(ok)?, payload(err)?]))
            }
            Contract::Enum(e) => AbiKind::Sum(SumLayout::Enum(
                e.tag(),
                e.cases.iter().map(|c| c.name.clone()).collect(),
            )),
            Contract::Variant(tag, cases) => {
                let mut payload = self.payloads(tag, cases.iter().map(|(_, ty)| ty.as_ref()));
                let cases = cases
                    .into_iter()
                    .map(|(name, ty)| Ok((name.to_owned(), payload(ty)?)))
                    .collect::<Result<_>>()?;
                AbiKind::Sum(SumLayout::Variant(tag, cases))
            }
            Contract::Unsupported(name) => return Err(unsupported(name)),
        };
        let mut space = [WasmType::I32; Resolve::MAX_FLAT_PARAMS];
        let mut slots = FlatTypes::new(&mut space);
        let flat = self
            .resolve
            .push_flat(&ty, &mut slots)
            .then(|| slots.to_vec().into_boxed_slice());
        Ok(Arc::new(AbiType {
            kind,
            flat,
            size: self.sizes.size(&ty).size_wasm32() as u32,
        }))
    }
}

fn unsupported(kind: &str) -> Diagnostic {
    diag!(
        crate::codes::UNSUPPORTED_TYPE,
        "WIT {kind} values are not supported by the compiler yet"
    )
}
