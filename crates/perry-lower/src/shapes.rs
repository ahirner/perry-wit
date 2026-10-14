//! HIR types to MIR types, with records identified by shape.
//!
//! TypeScript object types are structural, so a record is identified by its sorted fields and
//! their types: an interface `P { x; y }`, a type literal `{ y; x }`, and Perry's synthesized
//! `__AnonShape_*` class for the literal `{ y: …, x: … }` all map to `perry.record <shape_x_y>`.
//! A different structure with the same field names gets the next free suffix (`shape_x_y_2`).
//! Recursion is only possible through named types; a recursive interface gets a fresh name
//! before its fields are known. Field order inside the record is the sorted order; evaluation
//! order stays the source order.
//! Widening a record to a narrower shape (subset rule R2) is not supported yet.
//!
//! WIT types come from the SDK's generated declarations, which the parser does not read: the
//! compiler passes them as *foreign* HIR types under their SDK names (prefixed `wit:`), and imports of those
//! names resolve to them. Finite variants retain their case-specific payload types.
//! Options distinguish admitted null and undefined states; tuples use `{ _0: A, _1: B }`.
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_hir::{ir::Module as HirModule, types::Type as HirType};
use perry_mir::types::{ArrayType, OptionalType, PromiseType, RecordType, StringType};
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic};
use pliron::printable::Printable as _;
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::{TypeHandle, TypedHandle},
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};

pub(super) mod generics;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum DecoderInput {
    Snapshot,
    Cursor,
    Text,
}

/// The runtime carrier type of a binding, paired with its declared WIT integer domain when
/// annotated with a width-specific integer type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct BindingType {
    pub(super) carrier: TypeHandle,
    pub(super) integer: Option<TypedHandle<IntegerType>>,
}

impl BindingType {
    pub(super) const fn plain(carrier: TypeHandle) -> Self {
        Self {
            carrier,
            integer: None,
        }
    }

    pub(super) fn wit_type(self) -> TypeHandle {
        self.integer.map_or(self.carrier, Into::into)
    }

    pub(super) fn expected_type(self, ctx: &Context) -> TypeHandle {
        self.integer
            .filter(|int| int.deref(ctx).width() == 64)
            .map_or(self.carrier, Into::into)
    }
}

#[derive(Clone)]
enum PendingShape {
    Resolving,
    Recursive(String),
}

pub(super) struct Shapes<'h> {
    pub(super) module: pliron::builtin::ops::ModuleOp,
    pub(super) foreign_wrappers: RefCell<HashMap<(String, Vec<TypeHandle>), Identifier>>,
    pub(super) decoders: RefCell<HashMap<(TypeHandle, DecoderInput), Identifier>>,
    pub(super) globals: RefCell<HashMap<perry_hir::types::LocalId, BindingType>>,
    pub(super) global_ids: HashSet<perry_hir::types::LocalId>,
    pub(super) inferred_bindings: HashSet<perry_hir::types::LocalId>,
    pub(super) type_env: perry_hir::HirTypeEnv,
    pub(super) interfaces: HashMap<&'h str, &'h perry_hir::ir::Interface>,
    pub(super) type_aliases: HashMap<&'h str, &'h perry_hir::ir::TypeAlias>,
    user_classes: HashSet<&'h str>,
    object_literal_shapes: HashMap<&'h str, Vec<&'h str>>,
    function_names: HashSet<&'h str>,
    local_source_spans: &'h HashMap<perry_hir::types::LocalId, perry_hir::ir::LocalSourceSpan>,
    locations: &'h crate::locations::Locations<'h>,
    foreign: &'h HashMap<String, crate::hir::Foreign>,
    integer_imports: HashMap<&'h str, (u32, Signedness)>,
    imported_types: HashMap<&'h str, &'h crate::hir::Foreign>,
    foreign_functions: HashMap<&'h str, (String, &'h perry_hir::types::FunctionType)>,
    /// Named types whose fields are being computed, with the record name reserved when a
    /// field refers back to the type.
    pending: RefCell<HashMap<String, PendingShape>>,
    /// Record names reserved for recursive types.
    reserved: RefCell<usize>,
    /// Resolved named types, so a recursive interface keeps its reserved name.
    named: RefCell<HashMap<String, TypeHandle>>,
    inputs: RefCell<HashMap<TypeHandle, TypeHandle>>,
}

pub(super) fn identifier(name: &str) -> Result<Identifier> {
    Identifier::try_from(name.to_string()).map_err(|_| anyhow!("{name} is not a MIR identifier"))
}

fn record_name(fields: &[&str]) -> String {
    if fields.is_empty() {
        "shape_".to_string()
    } else {
        format!(
            "shape_{}",
            fields
                .join("_")
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                })
                .collect::<String>()
        )
    }
}

impl<'h> Shapes<'h> {
    pub(super) fn new(
        module: pliron::builtin::ops::ModuleOp,
        hir: &'h HirModule,
        foreign: &'h HashMap<String, crate::hir::Foreign>,
        inferred_bindings: std::collections::HashSet<perry_hir::types::LocalId>,
        locations: &'h crate::locations::Locations<'h>,
    ) -> Self {
        let mut integer_imports = HashMap::new();
        let mut imported_types = HashMap::new();
        let mut foreign_functions = HashMap::new();
        for import in &hir.imports {
            for specifier in &import.specifiers {
                let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                    continue;
                };
                if import.source == "perry:types"
                    && let Some(spec) = Self::perry_integer_spec(&format!("perry:types/{imported}"))
                {
                    integer_imports.insert(local.as_str(), spec);
                    continue;
                }
                let binding = format!("{}#{imported}", import.source);
                if let Some(crate::hir::Foreign::Function(signature)) = foreign.get(&binding) {
                    foreign_functions.insert(local.as_str(), (binding.clone(), signature));
                }
                if let Some(entry) = [binding.as_str(), imported.as_str()]
                    .into_iter()
                    .find_map(|key| foreign.get(key))
                    .filter(|entry| !matches!(entry, crate::hir::Foreign::Function(_)))
                {
                    imported_types.insert(local.as_str(), entry);
                }
            }
        }
        let interfaces = hir
            .interfaces
            .iter()
            .map(|interface| (interface.name.as_str(), interface))
            .collect();
        let type_aliases = hir
            .type_aliases
            .iter()
            .map(|alias| (alias.name.as_str(), alias))
            .collect();
        let mut user_classes = HashSet::new();
        let mut object_literal_shapes = HashMap::new();
        for class in &hir.classes {
            if class.name.starts_with("__AnonShape_") {
                object_literal_shapes.insert(
                    class.name.as_str(),
                    class.fields.iter().map(|f| f.name.as_str()).collect(),
                );
            } else {
                user_classes.insert(class.name.as_str());
            }
        }
        let function_names = hir
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        Shapes {
            module,
            decoders: RefCell::new(HashMap::new()),
            foreign_wrappers: RefCell::new(HashMap::new()),
            globals: RefCell::new(HashMap::new()),
            global_ids: if hir.functions.iter().any(|f| f.is_exported) {
                hir.init
                    .iter()
                    .filter_map(|s| match s {
                        perry_hir::ir::Stmt::Let { id, .. } => Some(*id),
                        _ => None,
                    })
                    .collect()
            } else {
                Default::default()
            },
            inferred_bindings,
            type_env: perry_hir::HirTypeEnv::from_module(hir),
            interfaces,
            type_aliases,
            user_classes,
            object_literal_shapes,
            function_names,
            local_source_spans: &hir.local_source_spans,
            locations,
            foreign,
            integer_imports,
            imported_types,
            foreign_functions,
            pending: RefCell::new(HashMap::new()),
            reserved: RefCell::new(0),
            named: RefCell::new(HashMap::new()),
            inputs: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn annotate_local(
        &self,
        error: anyhow::Error,
        id: perry_hir::types::LocalId,
    ) -> anyhow::Error {
        match self.local_source_spans.get(&id) {
            Some(span) => self.locations.annotate(
                error,
                swc_common::Span::new(
                    swc_common::BytePos(span.start),
                    swc_common::BytePos(span.end),
                ),
            ),
            None => error,
        }
    }

    pub(super) fn annotate_byte_offset(
        &self,
        error: anyhow::Error,
        byte_offset: u32,
        len: u32,
    ) -> anyhow::Error {
        if byte_offset == 0 {
            return error;
        }
        self.locations.annotate(
            error,
            swc_common::Span::new(
                swc_common::BytePos(byte_offset),
                swc_common::BytePos(byte_offset.saturating_add(len)),
            ),
        )
    }

    pub(super) fn annotate_call(&self, error: anyhow::Error, byte_offset: u32) -> anyhow::Error {
        self.locations.annotate_call(error, byte_offset)
    }

    pub(super) fn has_function(&self, name: &str) -> bool {
        self.function_names.contains(name)
    }

    pub(super) fn foreign_function(
        &self,
        local: &str,
    ) -> Option<(String, &perry_hir::types::FunctionType)> {
        self.foreign_functions
            .get(local)
            .map(|(binding, sig)| (binding.clone(), *sig))
    }

    /// MIR type of a HIR value type, for the types supported so far.
    pub(super) fn value_type(&self, ctx: &mut Context, ty: &HirType) -> Result<TypeHandle> {
        Ok(match ty {
            HirType::BigInt => perry_mir::types::BigIntType::get(ctx).into(),
            HirType::Number | HirType::Int32 => FP64Type::get(ctx).into(),
            HirType::Boolean => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            HirType::String | HirType::StringLiteral(_) => StringType::get(ctx).into(),
            HirType::Null | HirType::Void => {
                let inner = FP64Type::get(ctx).into();
                let absence = if matches!(ty, HirType::Null) {
                    perry_mir::attrs::Absence::Null
                } else {
                    perry_mir::attrs::Absence::Undefined
                };
                OptionalType::get_with(ctx, inner, absence).into()
            }
            HirType::Array(element) => {
                let element = self.value_type(ctx, element)?;
                ArrayType::get(ctx, element).into()
            }
            HirType::Named(name) => self.named(ctx, name)?,
            HirType::Generic { base, type_args } if base == "WitInput" => {
                let [inner] = type_args.as_slice() else {
                    bail!("WitInput needs one type argument");
                };
                let inner = self.value_type(ctx, inner)?;
                self.input_type(ctx, inner)?
            }
            HirType::Generic { base, type_args }
                if matches!(base.as_str(), "Map" | "ReadonlyMap") && !self.is_user_type(base) =>
            {
                let [k, v] = type_args.as_slice() else {
                    bail!("{base} needs key and value type arguments");
                };
                self.kv_array_type(ctx, k, v, "map_tuple")?
            }
            HirType::Generic { base, type_args }
                if base == "Record" && !self.is_user_type(base) =>
            {
                let [k, v] = type_args.as_slice() else {
                    bail!("Record needs key and value type arguments");
                };
                self.kv_array_type(ctx, k, v, "dict_tuple")?
            }
            HirType::Generic { base, type_args }
                if matches!(
                    base.as_str(),
                    "Set" | "ReadonlySet" | "ReadonlyArray" | "Array"
                ) && !self.is_user_type(base) =>
            {
                let [element] = type_args.as_slice() else {
                    bail!("{base} needs one element type argument");
                };
                let element = self.value_type(ctx, element)?;
                ArrayType::get(ctx, element).into()
            }
            HirType::Generic { base, type_args }
                if base == "ReadableStream"
                    && matches!(type_args.as_slice(), [HirType::Named(n)] if n == "Uint8Array")
                    && !self.is_user_type(base) =>
            {
                perry_mir::types::ReadableType::get(ctx).into()
            }
            HirType::Promise(inner) => {
                let settled = self.settled_type(ctx, inner)?;
                PromiseType::get(ctx, settled).into()
            }
            HirType::Generic { base, type_args } => self.generic(ctx, base, type_args)?,
            HirType::Object(object) => {
                if let Some(val_ty) = &object.index_signature {
                    ensure!(
                        object.properties.is_empty(),
                        "index signatures are outside the closed-record profile when combined with fixed properties"
                    );
                    return self.kv_array_type(ctx, &HirType::String, val_ty, "dict_tuple");
                }
                let fields = object
                    .properties
                    .iter()
                    .map(|(name, info)| (name.as_str(), &info.ty, info.optional))
                    .collect();
                self.hir_record(ctx, None, fields)?.into()
            }
            HirType::Union(members) => {
                if members
                    .iter()
                    .all(|m| matches!(m, HirType::StringLiteral(_)))
                {
                    return Ok(StringType::get(ctx).into());
                }
                let results: Option<Vec<&perry_hir::types::ObjectType>> = members
                    .iter()
                    .map(|member| match member {
                        HirType::Object(object)
                            if matches!(
                                object.properties.get("ok").map(|p| &p.ty),
                                Some(HirType::Boolean)
                            ) =>
                        {
                            Some(object)
                        }
                        _ => None,
                    })
                    .collect();
                if let Some(alternatives) = results {
                    return self.result(ctx, &alternatives);
                }
                if members.iter().any(|ty| {
                    matches!(ty, HirType::Number)
                        || self
                            .integer_type(ctx, ty)
                            .is_some_and(|int_ty| int_ty.deref(ctx).width() <= 32)
                }) && members.iter().any(|ty| matches!(ty, HirType::String))
                    && members.iter().all(|ty| {
                        matches!(ty, HirType::Number | HirType::String)
                            || self
                                .integer_type(ctx, ty)
                                .is_some_and(|int_ty| int_ty.deref(ctx).width() <= 32)
                    })
                {
                    return Ok(perry_mir::types::NumberStringType::get(ctx).into());
                }
                let defined: Vec<&HirType> = members
                    .iter()
                    .filter(|m| !matches!(m, HirType::Void | HirType::Null))
                    .collect();
                if defined.len() < members.len() {
                    // `T | undefined` (R8), `T | null` and `T | null | undefined`.
                    let inner = match defined.as_slice() {
                        [defined] => self.value_type(ctx, defined)?,
                        [] => bail!("an absence-only union needs a declared value type"),
                        defined => self.value_type(
                            ctx,
                            &HirType::Union(defined.iter().map(|ty| (*ty).clone()).collect()),
                        )?,
                    };
                    let absence = match (
                        members.iter().any(|m| matches!(m, HirType::Null)),
                        members.iter().any(|m| matches!(m, HirType::Void)),
                    ) {
                        (true, true) => perry_mir::attrs::Absence::Both,
                        (true, false) => perry_mir::attrs::Absence::Null,
                        _ => perry_mir::attrs::Absence::Undefined,
                    };
                    OptionalType::get_with(ctx, inner, absence).into()
                } else {
                    self.variant(ctx, &defined)?
                }
            }
            HirType::Tuple(elements) => {
                let fields = elements
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| {
                        let ty = if let Some(int_ty) = self.integer_type(ctx, ty)
                            && int_ty.deref(ctx).width() == 64
                        {
                            int_ty.into()
                        } else {
                            self.value_type(ctx, ty)?
                        };
                        Ok((format!("_{i}"), ty))
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.mir_record_kind(ctx, fields, true)?.into()
            }
            HirType::Any | HirType::Unknown => return Err(PerryDiagnostic::error(
                DiagnosticCode::MissingTypeAnnotation,
                "the value type cannot be inferred for the MIR backend",
            )
            .with_help(
                "Add a concrete type annotation to this binding, parameter, or function return.",
            )
            .into()),
            other => {
                return Err(PerryDiagnostic::error(
                    DiagnosticCode::UnsupportedType,
                    format!("MIR backend does not support type {other:?} yet"),
                )
                .into());
            }
        })
    }

    fn input_type(&self, ctx: &mut Context, ty: TypeHandle) -> Result<TypeHandle> {
        if let Some(mapped) = self.inputs.borrow().get(&ty) {
            return Ok(*mapped);
        }
        let deref = ty.deref(ctx);
        let mapped = if deref.is::<perry_mir::types::BytesType>() {
            drop(deref);
            perry_mir::types::ByteInputType::get(ctx).into()
        } else if let Some(array) = deref.downcast_ref::<ArrayType>() {
            let element = array.element();
            drop(deref);
            let element = self.input_type(ctx, element)?;
            ArrayType::get(ctx, element).into()
        } else if let Some(optional) = deref.downcast_ref::<OptionalType>() {
            let (value, absence) = (optional.value(), optional.absence());
            drop(deref);
            let value = self.input_type(ctx, value)?;
            OptionalType::get_with(ctx, value, absence).into()
        } else if let Some(variant) = deref.downcast_ref::<perry_mir::types::VariantType>() {
            let cases = variant.cases().to_vec();
            drop(deref);
            let cases = cases
                .into_iter()
                .map(|(tag, payload)| {
                    Ok((tag, payload.map(|ty| self.input_type(ctx, ty)).transpose()?))
                })
                .collect::<Result<_>>()?;
            perry_mir::types::VariantType::get(ctx, cases).into()
        } else if let Some(record) = deref.downcast_ref::<RecordType>() {
            let get = if record.is_tuple() {
                RecordType::get_tuple
            } else {
                RecordType::get
            };
            let name = identifier(&format!("input_{}", record.name()))?;
            let fields = record
                .fields()
                .map_err(|_| anyhow!("opaque input record"))?
                .to_vec();
            drop(deref);
            let mapped = get(ctx, name.clone(), None).map_err(|e| anyhow!("{}", e.disp(ctx)))?;
            self.inputs.borrow_mut().insert(ty, mapped.into());
            let fields = fields
                .into_iter()
                .map(|(name, ty)| Ok((name, self.input_type(ctx, ty)?)))
                .collect::<Result<_>>()?;
            get(ctx, name, Some(fields))
                .map_err(|e| anyhow!("{}", e.disp(ctx)))?
                .into()
        } else {
            ty
        };
        self.inputs.borrow_mut().insert(ty, mapped);
        Ok(mapped)
    }

    /// SDK results use disjoint, statically typed payload slots. The inactive slot is absent.
    fn result(
        &self,
        ctx: &mut Context,
        alternatives: &[&perry_hir::types::ObjectType],
    ) -> Result<TypeHandle> {
        ensure!(
            alternatives.len() == 2,
            "a tagged result needs exactly two alternatives"
        );
        let mut fields = vec![(
            "ok".to_string(),
            IntegerType::get(ctx, 1, Signedness::Signless).into(),
        )];
        for name in ["value", "error"] {
            let mut payload = None;
            for object in alternatives {
                ensure!(
                    object
                        .properties
                        .keys()
                        .all(|key| ["ok", "value", "error"].contains(&key.as_str())),
                    "result alternatives only contain ok, value and error"
                );
                if let Some(info) = object.properties.get(name) {
                    ensure!(
                        payload.is_none(),
                        "result payload {name} appears in both alternatives"
                    );
                    payload = Some(self.value_type(ctx, &info.ty)?);
                }
            }
            if let Some(ty) = payload {
                // Inactive payload fields must admit undefined, including nullable payloads.
                let ty = OptionalType::get(ctx, ty).into();
                fields.push((name.to_string(), ty));
            }
        }
        Ok(self.mir_record(ctx, fields)?.into())
    }

    fn variant(&self, ctx: &mut Context, members: &[&HirType]) -> Result<TypeHandle> {
        let mut cases = Vec::new();
        for member in members {
            let HirType::Object(object) = member else {
                bail!("unions other than tagged object types are not supported yet")
            };
            let Some(HirType::StringLiteral(tag)) = object.properties.get("tag").map(|p| &p.ty)
            else {
                bail!("union members need a literal `tag`")
            };
            ensure!(
                object.properties.keys().all(|k| k == "tag" || k == "val"),
                "union members have `tag` and `val` only"
            );
            ensure!(
                !cases.iter().any(|(name, _)| name == tag),
                "variant tags must be distinct"
            );
            let payload = object
                .properties
                .get("val")
                .map(|info| self.value_type(ctx, &info.ty))
                .transpose()?;
            cases.push((tag.clone(), payload));
        }
        Ok(perry_mir::types::VariantType::get(ctx, cases).into())
    }

    /// The value a promise of `ty` settles with; `void` settles `undefined`.
    pub(super) fn settled_type(&self, ctx: &mut Context, ty: &HirType) -> Result<TypeHandle> {
        match ty {
            HirType::Void => {
                let number = FP64Type::get(ctx).into();
                Ok(OptionalType::get(ctx, number).into())
            }
            ty => self.value_type(ctx, ty),
        }
    }

    pub(super) fn perry_integer_spec(name: &str) -> Option<(u32, Signedness)> {
        Some(match name {
            "perry:types/u8" => (8, Signedness::Unsigned),
            "perry:types/s8" => (8, Signedness::Signed),
            "perry:types/u16" => (16, Signedness::Unsigned),
            "perry:types/s16" => (16, Signedness::Signed),
            "perry:types/u32" => (32, Signedness::Unsigned),
            "perry:types/s32" => (32, Signedness::Signed),
            "perry:types/u64" => (64, Signedness::Unsigned),
            "perry:types/s64" => (64, Signedness::Signed),
            _ => return None,
        })
    }

    pub(super) fn perry_integer_type(
        ctx: &mut Context,
        name: &str,
    ) -> Option<TypedHandle<IntegerType>> {
        let (bits, signedness) = Self::perry_integer_spec(name)?;
        Some(IntegerType::get(ctx, bits, signedness))
    }

    pub(super) fn integer_type(
        &self,
        ctx: &mut Context,
        ty: &HirType,
    ) -> Option<TypedHandle<IntegerType>> {
        let mut seen = HashSet::new();
        self.integer_type_inner(ctx, ty, &mut seen)
    }

    fn integer_type_inner<'a>(
        &'a self,
        ctx: &mut Context,
        ty: &'a HirType,
        seen: &mut HashSet<&'a str>,
    ) -> Option<TypedHandle<IntegerType>> {
        let HirType::Named(name) = ty else {
            return None;
        };
        if let Some(int_ty) = Self::perry_integer_type(ctx, name) {
            return Some(int_ty);
        }
        if !seen.insert(name.as_str()) {
            return None;
        }
        if !self.is_user_type(name) {
            if let Some(&(width, signedness)) = self.integer_imports.get(name.as_str()) {
                return Some(IntegerType::get(ctx, width, signedness));
            }
            if let Some(entry) = self
                .imported_types
                .get(name.as_str())
                .copied()
                .or_else(|| self.foreign.get(name.as_str()))
            {
                return self.foreign_integer_type(ctx, entry, seen);
            }
        }
        if let Some(alias) = self
            .type_aliases
            .get(name.as_str())
            .filter(|a| a.type_params.is_empty())
        {
            return self.integer_type_inner(ctx, &alias.ty, seen);
        }
        None
    }

    fn foreign_integer_type<'a>(
        &'a self,
        ctx: &mut Context,
        entry: &'a crate::hir::Foreign,
        seen: &mut HashSet<&'a str>,
    ) -> Option<TypedHandle<IntegerType>> {
        match entry {
            crate::hir::Foreign::Integer { width, signedness } => {
                Some(IntegerType::get(ctx, *width, *signedness))
            }
            crate::hir::Foreign::Type(ty) => self.integer_type_inner(ctx, ty, seen),
            crate::hir::Foreign::Input(inner) => self.foreign_integer_type(ctx, inner, seen),
            crate::hir::Foreign::Resource(_) | crate::hir::Foreign::Function(_) => None,
        }
    }

    fn foreign_value_type(
        &self,
        ctx: &mut Context,
        entry: &crate::hir::Foreign,
    ) -> Result<TypeHandle> {
        match entry {
            crate::hir::Foreign::Type(ty) => self.value_type(ctx, ty),
            crate::hir::Foreign::Integer { width, .. } => Ok(if *width == 64 {
                perry_mir::types::BigIntType::get(ctx).into()
            } else {
                FP64Type::get(ctx).into()
            }),
            crate::hir::Foreign::Resource(id) => {
                Ok(perry_mir::types::ResourceType::get(ctx, *id).into())
            }
            crate::hir::Foreign::Input(inner) => {
                let inner = self.foreign_value_type(ctx, inner)?;
                self.input_type(ctx, inner)
            }
            crate::hir::Foreign::Function(_) => {
                bail!("foreign function cannot be used as a value type")
            }
        }
    }

    pub(super) fn is_explicit_binding(&self, id: perry_hir::types::LocalId) -> bool {
        self.local_source_spans.contains_key(&id) && !self.inferred_bindings.contains(&id)
    }

    /// Whether the program declares a type of this name (shadowing a built-in class).
    pub(super) fn is_user_type(&self, name: &str) -> bool {
        self.interfaces.contains_key(name)
            || self.type_aliases.contains_key(name)
            || self.user_classes.contains(name)
    }

    fn named(&self, ctx: &mut Context, name: &str) -> Result<TypeHandle> {
        if let Some(ty) = self.named.borrow().get(name) {
            return Ok(*ty);
        }
        let ty = self
            .resolve_named(ctx, name)
            .map_err(|error| self.locations.annotate_type(error, name))?;
        // A re-entrant reference returns the opaque record; only the outermost call caches.
        if !self.pending.borrow().contains_key(name) {
            self.named.borrow_mut().insert(name.to_string(), ty);
        }
        Ok(ty)
    }

    fn resolve_named(&self, ctx: &mut Context, name: &str) -> Result<TypeHandle> {
        if let Some((width, _)) = Self::perry_integer_spec(name) {
            return Ok(if width == 64 {
                perry_mir::types::BigIntType::get(ctx).into()
            } else {
                FP64Type::get(ctx).into()
            });
        }
        // Built-in classes the subset models (a user type or explicit import takes precedence).
        let user = self.interfaces.contains_key(name) || self.type_aliases.contains_key(name);
        if !user {
            if let Some(&(width, _)) = self.integer_imports.get(name) {
                return Ok(if width == 64 {
                    perry_mir::types::BigIntType::get(ctx).into()
                } else {
                    FP64Type::get(ctx).into()
                });
            }
            if let Some(entry) = self.imported_types.get(name) {
                return self.foreign_value_type(ctx, entry);
            }
            match name {
                "AbortController" | "AbortSignal" => {
                    return Ok(perry_mir::types::AbortType::get(ctx).into());
                }
                "Uint8Array" => return Ok(perry_mir::types::BytesType::get(ctx).into()),
                "URL" => return Ok(perry_mir::types::UrlType::get(ctx).into()),
                "URLSearchParams" => return Ok(perry_mir::types::SearchParamsType::get(ctx).into()),
                "Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError" => {
                    return Ok(perry_mir::types::ExceptionType::get(ctx).into());
                }
                "RegExp" => return Ok(perry_mir::types::RegexType::get(ctx).into()),
                "Date" => return Ok(perry_mir::types::DateType::get(ctx).into()),
                "Temporal.Instant" => return Ok(perry_mir::types::InstantType::get(ctx).into()),
                "Temporal.PlainDate" => {
                    return Ok(perry_mir::types::PlainDateType::get(ctx).into());
                }
                "Temporal.PlainDateTime" => {
                    return Ok(perry_mir::types::PlainDateTimeType::get(ctx).into());
                }

                "Headers" => return Ok(perry_mir::types::HeadersType::get(ctx).into()),
                "Response" => return Ok(perry_mir::types::ResponseType::get(ctx).into()),
                "Request" => return Ok(perry_mir::types::RequestType::get(ctx).into()),
                // Byte streams: `ReadableStream<Uint8Array>` and its default reader.
                "ReadableStream" => return Ok(perry_mir::types::ReadableType::get(ctx).into()),
                "ArrayBuffer" => return Ok(perry_mir::types::ByteBufferType::get(ctx).into()),
                "ReadableStreamBYOBReader" => {
                    return Ok(perry_mir::types::ByobReaderType::get(ctx).into());
                }
                "ReadableStreamDefaultReader" => {
                    return Ok(perry_mir::types::ReaderType::get(ctx).into());
                }
                "TextDecoder" => return Ok(perry_mir::types::DecoderType::get(ctx).into()),
                _ => {}
            }
            if let Some(entry) = self.foreign.get(name) {
                return self.foreign_value_type(ctx, entry);
            }
        }
        if let Some(interface) = self.interfaces.get(name) {
            if !interface.type_params.is_empty() {
                return self.generic(ctx, name, &[]);
            }
            ensure!(
                interface.methods.is_empty(),
                "interface {name}: methods are not supported yet"
            );
            let lookup = (&self.interfaces, &self.type_aliases);
            let properties =
                generics::collect_interface_properties(&lookup, interface, &HashMap::new(), 0)
                    .with_context(|| {
                        format!("failed to resolve inherited properties of interface {name}")
                    })?;
            let fields = properties
                .iter()
                .map(|(n, ty, opt)| (n.as_str(), ty, *opt))
                .collect();
            return Ok(self.hir_record(ctx, Some(name), fields)?.into());
        }
        if let Some(alias) = self.type_aliases.get(name) {
            if !alias.type_params.is_empty() {
                return self.generic(ctx, name, &[]);
            }
            return self.value_type(ctx, &alias.ty);
        }
        bail!("type {name} is not supported by the MIR backend yet")
    }

    /// A record from HIR field types; `named` is the interface they come from, if any.
    fn hir_record(
        &self,
        ctx: &mut Context,
        named: Option<&str>,
        mut fields: Vec<(&str, &HirType, bool)>,
    ) -> Result<TypedHandle<RecordType>> {
        fields.sort_by(|a, b| a.0.cmp(b.0));
        let names: Vec<&str> = fields.iter().map(|(name, _, _)| *name).collect();
        if let Some(named) = named {
            let reentry = self.pending.borrow().get(named).cloned();
            match reentry {
                Some(PendingShape::Recursive(reserved)) => {
                    return RecordType::get(ctx, identifier(&reserved)?, None)
                        .map_err(|e| anyhow!("{}", e.disp(ctx)));
                }
                Some(PendingShape::Resolving) => {
                    let mut count = self.reserved.borrow_mut();
                    *count += 1;
                    let reserved = format!("{}_rec{}", record_name(&names), *count);
                    self.pending
                        .borrow_mut()
                        .insert(named.to_string(), PendingShape::Recursive(reserved.clone()));
                    return RecordType::get(ctx, identifier(&reserved)?, None)
                        .map_err(|e| anyhow!("{}", e.disp(ctx)));
                }
                None => {
                    self.pending
                        .borrow_mut()
                        .insert(named.to_string(), PendingShape::Resolving);
                }
            }
        }
        let typed = fields
            .iter()
            .map(|(field, ty, optional)| {
                let ty = if !*optional
                    && let Some(int_ty) = self.integer_type(ctx, ty)
                    && int_ty.deref(ctx).width() == 64
                {
                    int_ty.into()
                } else {
                    self.value_type(ctx, ty)
                        .with_context(|| format!("field {field}"))?
                };
                let ty = if *optional {
                    OptionalType::get(ctx, ty).into()
                } else {
                    ty
                };
                Ok((field.to_string(), ty))
            })
            .collect::<Result<Vec<_>>>();
        let reserved = named.and_then(|named| match self.pending.borrow_mut().remove(named) {
            Some(PendingShape::Recursive(name)) => Some(name),
            _ => None,
        });
        let typed = typed?;
        match reserved {
            Some(name) => RecordType::get(ctx, identifier(&name)?, Some(typed))
                .map_err(|e| anyhow!("{}", e.disp(ctx))),
            None => self.mir_record(ctx, typed),
        }
    }

    /// A record from MIR field types, in any order.
    pub(super) fn mir_record(
        &self,
        ctx: &mut Context,
        fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_record_prefix(ctx, fields, None)
    }

    pub(super) fn kv_array_type(
        &self,
        ctx: &mut Context,
        k: &HirType,
        v: &HirType,
        prefix: &str,
    ) -> Result<TypeHandle> {
        let k_ty = self.value_type(ctx, k)?;
        let v_ty = self.value_type(ctx, v)?;
        let entry = self.mir_record_prefix(
            ctx,
            vec![("_0".to_string(), k_ty), ("_1".to_string(), v_ty)],
            Some(prefix),
        )?;
        Ok(ArrayType::get(ctx, entry.into()).into())
    }

    fn mir_record_kind(
        &self,
        ctx: &mut Context,
        fields: Vec<(String, TypeHandle)>,
        tuple: bool,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_record_prefix(ctx, fields, tuple.then_some("tuple"))
    }

    pub(super) fn mir_record_prefix(
        &self,
        ctx: &mut Context,
        mut fields: Vec<(String, TypeHandle)>,
        prefix: Option<&str>,
    ) -> Result<TypedHandle<RecordType>> {
        let tuple = prefix.is_some();
        if !tuple {
            fields.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
        let base = record_name(&names);
        let base = match prefix {
            Some(p) => format!("{p}_{base}"),
            None => base,
        };
        let get = if tuple {
            RecordType::get_tuple
        } else {
            RecordType::get
        };
        // The first name whose definition matches, or which is still free.
        let mut suffix = 1;
        loop {
            let name = if suffix == 1 {
                base.clone()
            } else {
                format!("{base}_{suffix}")
            };
            if let Ok(record) = get(ctx, identifier(&name)?, Some(fields.clone())) {
                return Ok(record);
            }
            suffix += 1;
        }
    }

    /// Field names, in source order, of a synthesized object-literal shape.
    pub(super) fn literal_fields(&self, class_name: &str) -> Option<&[&'h str]> {
        self.object_literal_shapes
            .get(class_name)
            .map(Vec::as_slice)
    }
}
