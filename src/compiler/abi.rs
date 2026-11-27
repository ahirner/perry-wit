//! Canonical wasm32 layouts at the WIT binding boundary.
use perry_codegen::boundary::{AbiKind, AbiType, Flat};
use perry_contract::{
    names::canonical_type,
    types::{Contract, project},
};
use perry_wit_diagnostics::{Diagnostic, Result, diag};
use wit_parser::{
    Function, Int, LiftLowerAbi, ManglingAndAbi, Resolve, SizeAlign, Type,
    abi::{FlatTypes, WasmSignature, WasmType},
};

pub(super) struct Layouts<'r> {
    resolve: &'r Resolve,
    sizes: SizeAlign,
}

impl<'r> Layouts<'r> {
    pub(super) fn new(resolve: &'r Resolve) -> Result<Self> {
        let mut sizes = SizeAlign::default();
        sizes
            .fill(resolve)
            .map_err(|e| Diagnostic::internal(e.to_string()))?;
        Ok(Self { resolve, sizes })
    }
    pub(super) fn layout(&self, ty: Type, kind: AbiKind) -> AbiType {
        let mut space = [WasmType::I32; Resolve::MAX_FLAT_PARAMS];
        let mut slots = FlatTypes::new(&mut space);
        let flat = self
            .resolve
            .push_flat(&ty, &mut slots)
            .then(|| slots.to_vec().into_iter().map(Flat::from).collect());
        AbiType {
            kind,
            flat,
            size: self.sizes.size(&ty).size_wasm32() as u32,
            offset: 0,
        }
    }

    fn fields<'a>(&self, types: impl Iterator<Item = &'a Type>) -> Result<Vec<AbiType>> {
        self.sizes
            .field_offsets(types)
            .into_iter()
            .map(|(offset, ty)| self.value_at(*ty, offset.size_wasm32() as u32))
            .collect()
    }

    fn payload_offset<'a>(
        &self,
        tag: Int,
        types: impl IntoIterator<Item = Option<&'a Type>>,
    ) -> u32 {
        self.sizes.payload_offset(tag, types).size_wasm32() as u32
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
        &self,
        function: &Function,
        core: &WasmSignature,
    ) -> Result<(Vec<AbiType>, Option<u32>)> {
        let types = || function.params.iter().map(|p| &p.ty);
        let params = self.fields(types())?;
        let area = core
            .indirect_params
            .then(|| self.sizes.record(types()).size.size_wasm32() as u32);
        Ok((params, area))
    }
    pub(super) fn value(&self, ty: Type) -> Result<AbiType> {
        self.value_at(ty, 0)
    }

    fn value_at(&self, mut ty: Type, offset: u32) -> Result<AbiType> {
        let payload = |ty: Option<Type>, at| ty.map(|t| self.value_at(t, at)).transpose();
        let kind = match project(self.resolve, ty, |ty| *ty) {
            Contract::Alias(inner) => return self.value_at(inner, offset),
            Contract::Scalar(Type::Char) => return Err(unsupported("char")),
            Contract::Scalar(Type::ErrorContext) => return Err(unsupported("error-context")),
            Contract::Scalar(Type::String) => AbiKind::String,
            Contract::Scalar(scalar) => AbiKind::Scalar(scalar),
            Contract::Resource { id, owned } => {
                ty = Type::U32;
                AbiKind::Resource {
                    key: canonical_type(self.resolve, id).index() as u32,
                    owned,
                }
            }
            Contract::ByteStream => AbiKind::ByteStream,
            Contract::Bytes => AbiKind::List(Box::new(self.value(Type::U8)?)),
            Contract::List(item) => AbiKind::List(Box::new(self.value(item)?)),
            Contract::Tuple(types) => {
                let names = (0..types.len()).map(|i| format!("_{i}"));
                AbiKind::Record(names.zip(self.fields(types.iter())?).collect())
            }
            Contract::Record(fields) => {
                let layouts = self.fields(fields.iter().map(|(_, ty)| ty))?;
                AbiKind::Record(
                    fields
                        .into_iter()
                        .map(|(name, _)| name)
                        .zip(layouts)
                        .collect(),
                )
            }
            Contract::Option(item) => {
                let at = self.payload_offset(Int::U8, [Some(&item)]);
                AbiKind::Option(Box::new(self.value_at(item, at)?))
            }
            Contract::Result { ok, err } => {
                let at = self.payload_offset(Int::U8, [ok.as_ref(), err.as_ref()]);
                AbiKind::Result {
                    ok: payload(ok, at)?.map(Box::new),
                    err: payload(err, at)?.map(Box::new),
                }
            }
            Contract::Enum(e) => {
                AbiKind::Enum(e.tag(), e.cases.iter().map(|c| c.name.clone()).collect())
            }
            Contract::Variant(tag, cases) => {
                let at = self.payload_offset(tag, cases.iter().map(|(_, ty)| ty.as_ref()));
                AbiKind::Variant(
                    tag,
                    cases
                        .into_iter()
                        .map(|(name, ty)| Ok((name.to_owned(), payload(ty, at)?)))
                        .collect::<Result<_>>()?,
                )
            }
            Contract::Unsupported(name) => return Err(unsupported(name)),
        };
        let mut abi = self.layout(ty, kind);
        abi.offset = offset;
        Ok(abi)
    }
}

fn unsupported(kind: &str) -> Diagnostic {
    diag!(
        crate::codes::UNSUPPORTED_TYPE,
        "WIT {kind} values are not supported by the compiler yet"
    )
}
