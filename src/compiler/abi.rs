//! Canonical wasm32 layouts at the WIT binding boundary.
use perry_codegen::boundary::{AbiKind, AbiType, Flat};
use perry_contract::names::{canonical_type, to_camel_case};
use perry_wit_diagnostics::{Diagnostic, Result, diag};
use wit_parser::{
    Function, Handle, Int, LiftLowerAbi, ManglingAndAbi, Resolve, SizeAlign, Type, TypeDefKind,
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
        let resource = |id, owned| AbiKind::Resource {
            key: canonical_type(self.resolve, id).index() as u32,
            owned,
        };
        let payload = |ty: Option<Type>, at| ty.map(|t| self.value_at(t, at)).transpose();
        let kind = match ty {
            Type::Char => return Err(unsupported("char")),
            Type::ErrorContext => return Err(unsupported("error-context")),
            Type::String => AbiKind::String,
            Type::Id(id) => match &self.resolve.types[id].kind {
                TypeDefKind::Type(inner) => return self.value_at(*inner, offset),
                TypeDefKind::Resource => {
                    ty = Type::U32;
                    resource(id, true)
                }
                TypeDefKind::Handle(Handle::Own(id)) => resource(*id, true),
                TypeDefKind::Handle(Handle::Borrow(id)) => resource(*id, false),
                TypeDefKind::Stream(Some(Type::U8)) => AbiKind::ByteStream,
                TypeDefKind::List(t) => AbiKind::List(Box::new(self.value(*t)?)),
                TypeDefKind::Tuple(t) => AbiKind::Record(
                    self.fields(t.types.iter())?
                        .into_iter()
                        .enumerate()
                        .map(|(i, t)| (format!("_{i}"), t))
                        .collect(),
                ),
                TypeDefKind::Record(r) => {
                    let fields = self.fields(r.fields.iter().map(|f| &f.ty))?;
                    AbiKind::Record(
                        r.fields
                            .iter()
                            .map(|f| to_camel_case(&f.name))
                            .zip(fields)
                            .collect(),
                    )
                }
                TypeDefKind::Option(t) => {
                    let at = self.sizes.payload_offset(Int::U8, [Some(t)]).size_wasm32() as u32;
                    AbiKind::Option(Box::new(self.value_at(*t, at)?))
                }
                TypeDefKind::Result(r) => {
                    let at = self
                        .sizes
                        .payload_offset(Int::U8, [r.ok.as_ref(), r.err.as_ref()])
                        .size_wasm32() as u32;
                    AbiKind::Result {
                        ok: payload(r.ok, at)?.map(Box::new),
                        err: payload(r.err, at)?.map(Box::new),
                    }
                }
                TypeDefKind::Enum(e) => {
                    AbiKind::Enum(e.tag(), e.cases.iter().map(|c| c.name.clone()).collect())
                }
                TypeDefKind::Variant(v) => {
                    let at = self
                        .sizes
                        .payload_offset(v.tag(), v.cases.iter().map(|c| c.ty.as_ref()))
                        .size_wasm32() as u32;
                    AbiKind::Variant(
                        v.tag(),
                        v.cases
                            .iter()
                            .map(|c| Ok((c.name.clone(), payload(c.ty, at)?)))
                            .collect::<Result<_>>()?,
                    )
                }
                other => return Err(unsupported(other.as_str())),
            },
            scalar => AbiKind::Scalar(scalar),
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
