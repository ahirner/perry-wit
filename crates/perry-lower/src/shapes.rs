//! Map frontend declarations and record identities to MIR representations.
//!
//! Frontend record identities map to carriers; object construction evaluates fields in source order.
use crate::{LowerError, ShapeError};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_frontend::hir::{ir::Module as HirModule, types::Type as HirType};
use perry_mir::{ArrayType, OptionalType, PromiseType, RecordKind, RecordType, StringType};
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

mod collection_types;
pub(crate) use perry_frontend::shapes::TypeId;
use perry_frontend::shapes::{Classes, Declarations, RecordId, captured, generics::InstanceKey};

mod generics;
mod representation;
use perry_frontend::shapes::perry_integer_spec;
use representation::integer_mir_carrier;

fn fresh_record(
    ctx: &Context,
    kind: RecordKind,
    fields: Option<Vec<(String, TypeHandle)>>,
) -> Result<TypedHandle<RecordType>> {
    RecordType::fresh(ctx, kind, fields).map_err(|error| anyhow!("{}", error.disp(ctx)))
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

#[derive(Clone, Copy)]
enum PendingShape {
    Resolving,
    Recursive(TypedHandle<RecordType>),
}

pub(super) struct Shapes<'h> {
    pub(super) module: pliron::builtin::ops::ModuleOp,
    pub(super) foreign_wrappers: RefCell<HashMap<(&'h str, Vec<TypeHandle>), Identifier>>,
    pub(super) globals: RefCell<HashMap<perry_frontend::hir::types::LocalId, BindingType>>,
    pub(super) global_ids: HashSet<perry_frontend::hir::types::LocalId>,
    pub(super) type_env: perry_frontend::hir::HirTypeEnv,
    pub(super) decls: Declarations<'h>,
    pub(super) classes: Classes<'h>,
    pub(super) static_fields: HashMap<
        (&'h str, &'h str),
        (
            perry_frontend::hir::types::LocalId,
            &'h perry_frontend::hir::ir::ClassField,
        ),
    >,
    pub(super) class_records: RefCell<HashMap<TypeHandle, (&'h str, Vec<TypeId>)>>,
    function_names: HashSet<&'h str>,
    local_source_spans:
        &'h HashMap<perry_frontend::hir::types::LocalId, perry_frontend::hir::ir::LocalSourceSpan>,
    pub(super) metadata: &'h crate::hir_compat::Metadata<'h>,
    /// Named types whose fields are being computed, with a fresh record handle reserved when a
    /// field refers back to the type.
    pending: RefCell<HashMap<InstanceKey, PendingShape>>,
    /// Resolved nominal instances keyed by declaration identity and type arguments.
    named: RefCell<HashMap<InstanceKey, TypeHandle>>,
    records: RefCell<HashMap<RecordId, TypedHandle<RecordType>>>,
    inputs: RefCell<HashMap<TypeHandle, TypeHandle>>,
}

pub(super) fn identifier(name: &str) -> Result<Identifier> {
    Identifier::try_from(name.to_string()).map_err(|_| anyhow!("{name} is not a MIR identifier"))
}

impl<'h> Shapes<'h> {
    pub(super) fn new(
        module: pliron::builtin::ops::ModuleOp,
        hir: &'h HirModule,
        foreign: &'h HashMap<String, crate::hir::Foreign>,
        metadata: &'h crate::hir_compat::Metadata<'h>,
    ) -> Self {
        let decls = Declarations::new(
            &hir.interfaces,
            &hir.type_aliases,
            &hir.imports,
            &hir.classes,
            foreign,
        );
        let classes = Classes::new(&hir.classes);
        let mut static_fields = HashMap::new();
        let mut next_static_id = 0x8000_0000u32;
        for class in hir
            .classes
            .iter()
            .filter(|class| classes.named.contains_key(class.name.as_str()))
        {
            for field in &class.static_fields {
                static_fields.insert(
                    (class.name.as_str(), field.name.as_str()),
                    (next_static_id, field),
                );
                next_static_id += 1;
            }
        }
        let function_names = hir
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        let mut global_ids = captured(hir);
        if !hir.functions.is_empty() || !classes.named.is_empty() {
            global_ids.extend(static_fields.values().map(|(id, _)| *id));
        }
        Shapes {
            module,
            foreign_wrappers: RefCell::new(HashMap::new()),
            globals: RefCell::new(HashMap::new()),
            global_ids,
            type_env: perry_frontend::hir::HirTypeEnv::from_module(hir),
            decls,
            classes,
            static_fields,
            class_records: RefCell::new(HashMap::new()),
            function_names,
            local_source_spans: &hir.local_source_spans,
            metadata,
            pending: RefCell::new(HashMap::new()),
            named: RefCell::new(HashMap::new()),
            records: RefCell::new(HashMap::new()),
            inputs: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn annotate_local(
        &self,
        error: anyhow::Error,
        id: perry_frontend::hir::types::LocalId,
    ) -> anyhow::Error {
        match self.local_source_spans.get(&id) {
            Some(span) => crate::diagnostics::annotate(
                error,
                self.metadata
                    .locations
                    .offset_span(span.start, span.end.saturating_sub(span.start)),
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
        crate::diagnostics::annotate(error, self.metadata.locations.offset_span(byte_offset, len))
    }

    pub(super) fn annotate_call(&self, error: anyhow::Error, byte_offset: u32) -> anyhow::Error {
        crate::diagnostics::annotate(error, self.metadata.locations.call_span(byte_offset))
    }

    pub(super) fn has_function(&self, name: &str) -> bool {
        self.function_names.contains(name)
    }

    pub(super) fn foreign_function(
        &self,
        local: &str,
    ) -> Option<(&'h str, &'h perry_frontend::hir::types::FunctionType)> {
        self.decls.foreign_functions.get(local).copied()
    }

    pub(super) fn input_type(&self, ctx: &Context, ty: TypeHandle) -> Result<TypeHandle> {
        if let Some(mapped) = self.inputs.borrow().get(&ty) {
            return Ok(*mapped);
        }
        let deref = ty.deref(ctx);
        let mapped = if deref.is::<perry_mir::BytesType>() {
            drop(deref);
            perry_mir::ByteInputType::get(ctx).into()
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
        } else if let Some(variant) = deref.downcast_ref::<perry_mir::VariantType>() {
            let cases = variant.cases().to_vec();
            drop(deref);
            let cases = cases
                .into_iter()
                .map(|(tag, payload)| {
                    Ok((tag, payload.map(|ty| self.input_type(ctx, ty)).transpose()?))
                })
                .collect::<Result<_>>()?;
            perry_mir::VariantType::get(ctx, cases).into()
        } else if let Some(record) = deref.downcast_ref::<RecordType>() {
            let kind = record.kind();
            let fields = record
                .fields()
                .map_err(|_| anyhow!("opaque input record"))?
                .to_vec();
            drop(deref);
            let mapped = fresh_record(ctx, kind, None)?;
            self.inputs.borrow_mut().insert(ty, mapped.into());
            let fields = fields
                .into_iter()
                .map(|(name, ty)| Ok((name, self.input_type(ctx, ty)?)))
                .collect::<Result<_>>()?;
            let name = mapped.deref(ctx).name().clone();
            RecordType::get_kind(ctx, name, Some(fields), kind)
                .map_err(|e| anyhow!("{}", e.disp(ctx)))?
                .into()
        } else {
            ty
        };
        self.inputs.borrow_mut().insert(ty, mapped);
        Ok(mapped)
    }

    pub(super) fn is_explicit_binding(&self, id: perry_frontend::hir::types::LocalId) -> bool {
        self.local_source_spans.contains_key(&id) && !self.metadata.inferred_bindings.contains(&id)
    }

    /// Whether the program declares a type of this name (shadowing a built-in class).
    pub(super) fn is_user_type(&self, name: &str) -> bool {
        self.decls.is_user_type(name)
    }

    pub(super) fn class_of_type(
        &self,
        ctx: &Context,
        ty: TypeHandle,
    ) -> Option<(&'h str, Vec<TypeId>)> {
        let inner = crate::present_type(ctx, ty);
        self.class_records.borrow().get(&inner).cloned()
    }

    pub(super) fn static_field(
        &self,
        class_name: &str,
        field_name: &str,
    ) -> Option<(
        perry_frontend::hir::types::LocalId,
        &'h perry_frontend::hir::ir::ClassField,
    )> {
        self.static_fields.get(&(class_name, field_name)).copied()
    }

    fn class_member(
        &self,
        class_name: &str,
        find: impl FnOnce(
            &'h perry_frontend::hir::ir::Class,
        ) -> Option<&'h perry_frontend::hir::ir::Function>,
    ) -> Option<(&'h str, &'h perry_frontend::hir::ir::Function)> {
        let (&owner_name, &(_, class)) = self.classes.named.get_key_value(class_name)?;
        Some((owner_name, find(class)?))
    }

    pub(super) fn class_method(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<(&'h str, &'h perry_frontend::hir::ir::Function)> {
        let norm = crate::hir_compat::method(method_name).name();
        self.class_member(class_name, |c| c.methods.iter().find(|m| m.name == norm))
    }

    pub(super) fn class_static_method(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<(&'h str, &'h perry_frontend::hir::ir::Function)> {
        let norm = crate::hir_compat::method(method_name).name();
        self.class_member(class_name, |c| {
            c.static_methods.iter().find(|m| m.name == norm)
        })
    }

    pub(super) fn class_getter(
        &self,
        class_name: &str,
        prop_name: &str,
    ) -> Option<(&'h str, &'h perry_frontend::hir::ir::Function)> {
        self.class_member(class_name, |c| {
            c.getters
                .iter()
                .find(|(n, _)| n == prop_name)
                .map(|(_, f)| f)
        })
    }

    pub(super) fn class_setter(
        &self,
        class_name: &str,
        prop_name: &str,
    ) -> Option<(&'h str, &'h perry_frontend::hir::ir::Function)> {
        self.class_member(class_name, |c| {
            c.setters
                .iter()
                .find(|(n, _)| n == prop_name)
                .map(|(_, f)| f)
        })
    }

    pub(super) fn named(&self, ctx: &Context, name: &str) -> Result<TypeHandle> {
        self.resolve_named(ctx, name).map_err(|error| {
            crate::diagnostics::annotate(error, self.metadata.locations.type_span(name))
        })
    }

    fn resolve_named(&self, ctx: &Context, name: &str) -> Result<TypeHandle> {
        if let Some((width, _)) = name
            .strip_prefix("perry:types/")
            .and_then(perry_integer_spec)
        {
            return Ok(integer_mir_carrier(ctx, width));
        }
        // Built-in classes the subset models (a user type or explicit import takes precedence).
        let user = self.is_user_type(name);
        if !user {
            if let Some(&(width, _)) = self.decls.integer_imports.get(name) {
                return Ok(integer_mir_carrier(ctx, width));
            }
            if let Some(entry) = self.decls.imported_types.get(name) {
                return self.foreign_value_type(ctx, entry);
            }
            if let Some(ty) = perry_mir::builtin_value_type(ctx, name) {
                return Ok(ty);
            }
            if let Some(entry) = self.decls.foreign.get(name) {
                return self.foreign_value_type(ctx, entry);
            }
        }
        if user {
            return self.generic(ctx, name, &[]);
        }
        bail!("type {name} is not supported by the MIR backend yet")
    }

    /// Reserve recursive identity before resolving source fields.
    fn source_record(
        &self,
        ctx: &Context,
        named: Option<&InstanceKey>,
        fields: Vec<(&str, &HirType, bool)>,
    ) -> Result<TypedHandle<RecordType>> {
        let mut fields: Vec<_> = fields
            .into_iter()
            .map(|(name, ty, optional)| (name.to_owned(), TypeId::intern(ctx, ty), optional))
            .collect();
        let identity = self.decls.record_id(named, &mut fields);
        if let Some(record) = self.records.borrow().get(&identity) {
            return Ok(*record);
        }
        if let Some(named) = named {
            let reentry = self.pending.borrow().get(named).copied();
            match reentry {
                Some(PendingShape::Recursive(reserved)) => return Ok(reserved),
                Some(PendingShape::Resolving) => {
                    let reserved = fresh_record(ctx, RecordKind::Object, None)?;
                    self.records.borrow_mut().insert(identity, reserved);
                    self.pending
                        .borrow_mut()
                        .insert(named.clone(), PendingShape::Recursive(reserved));
                    return Ok(reserved);
                }
                None => {
                    self.pending
                        .borrow_mut()
                        .insert(named.clone(), PendingShape::Resolving);
                }
            }
        }
        let typed = fields
            .iter()
            .map(|(field, ty, optional)| {
                let ty = ty.hir(ctx);
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
            Some(PendingShape::Recursive(record)) => Some(record),
            _ => None,
        });
        let typed = typed?;
        let record = match reserved {
            Some(record) => {
                let name = record.deref(ctx).name().clone();
                RecordType::get(ctx, name, Some(typed)).map_err(|e| anyhow!("{}", e.disp(ctx)))
            }
            None if let Some(record) = self.records.borrow().get(&identity) => Ok(*record),
            None if identity.is_nominal() => fresh_record(ctx, RecordKind::Object, Some(typed)),
            None => self.mir_record(ctx, typed),
        }?;
        self.records.borrow_mut().insert(identity, record);
        Ok(record)
    }

    /// A record from MIR field types, in any order.
    pub(super) fn mir_record(
        &self,
        ctx: &Context,
        fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_record_kind(ctx, RecordKind::Object, fields)
    }

    pub(super) fn mir_tuple(
        &self,
        ctx: &Context,
        fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_record_kind(ctx, RecordKind::Tuple, fields)
    }

    pub(super) fn mir_record_kind(
        &self,
        ctx: &Context,
        kind: RecordKind,
        fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        RecordType::structural(ctx, kind, fields).map_err(|error| anyhow!("{}", error.disp(ctx)))
    }

    /// Field names, in source order, of a synthesized object-literal shape.
    pub(super) fn literal_fields(&self, class_name: &str) -> Option<&[&'h str]> {
        self.classes.literal_fields(class_name)
    }
}
