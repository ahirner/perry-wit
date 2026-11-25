//! Map frontend declarations and record identities to MIR representations.
//!
//! Frontend record identities map to carriers; object construction evaluates fields in source order.
use crate::lower::{
    Diagnostic, Result, bail_diag, codes, diag, ensure_diag, ensure_unsupported,
    missing_type_annotation, unsupported,
};
use perry_hir::{
    ir::{Expr, Module as HirModule},
    types::Type as HirType,
};
use perry_mir::{
    ArrayType, OptionalType, PlironResultExt as _, PromiseType, RecordKind, RecordType, StringType,
};
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
use crate::engine::types::{Ty, TyKind};
use crate::shapes::{
    Declarations, captured,
    generics::{InstanceKey, Properties, object_properties},
};

mod generics;
mod representation;
mod resolved;
mod unions;
use crate::shapes::perry_integer_spec;
use representation::integer_mir_carrier;

fn fresh_record(
    ctx: &Context,
    kind: RecordKind,
    fields: Option<Vec<(String, TypeHandle)>>,
) -> Result<TypedHandle<RecordType>> {
    RecordType::fresh(ctx, kind, fields).to_diag(ctx)
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BindingAnnotation {
    Inferred,
    Explicit,
}

#[derive(Clone, Copy)]
enum PendingShape {
    Resolving,
    Recursive(TypedHandle<RecordType>),
}

#[derive(Clone, Copy)]
pub(super) struct StaticField<'h> {
    pub(super) id: perry_hir::types::LocalId,
    pub(super) field: &'h perry_hir::ir::ClassField,
    pub(super) ty: Option<crate::engine::types::Ty>,
}

pub(super) struct Shapes<'h> {
    pub(super) specialization: &'h crate::engine::Specialization,
    pub(super) module: pliron::builtin::ops::ModuleOp,
    pub(super) foreign_wrappers: RefCell<HashMap<(&'h str, Vec<TypeHandle>), Identifier>>,
    pub(super) globals: RefCell<HashMap<perry_hir::types::LocalId, BindingType>>,
    pub(super) global_ids: HashSet<perry_hir::types::LocalId>,
    pub(super) decls: Declarations<'h>,
    pub(super) static_fields: HashMap<(&'h str, &'h str), StaticField<'h>>,
    runtime_layouts: HashMap<&'h str, &'h crate::engine::RecordLayout>,
    resolved: RefCell<HashMap<crate::engine::types::Ty, BindingType>>,
    runtime_records: RefCell<HashMap<&'h str, TypedHandle<RecordType>>>,
    class_records: RefCell<HashMap<TypeHandle, &'h str>>,
    pub(super) lexical_functions: HashMap<perry_hir::types::FuncId, &'h Expr>,
    function_names: HashSet<&'h str>,
    local_source_spans: &'h HashMap<perry_hir::types::LocalId, perry_hir::ir::LocalSourceSpan>,
    pub(super) metadata: &'h crate::hir_compat::Metadata<'h>,
    /// Named types whose fields are being computed, with a fresh record handle reserved when a
    /// field refers back to the type.
    pending: RefCell<HashMap<InstanceKey, PendingShape>>,
    /// Resolved nominal instances keyed by declaration identity and type arguments.
    named: RefCell<HashMap<InstanceKey, TypeHandle>>,
    structural_records: RefCell<HashMap<Properties, TypedHandle<RecordType>>>,
    inputs: RefCell<HashMap<TypeHandle, TypeHandle>>,
}

pub(super) fn identifier(name: &str) -> Result<Identifier> {
    Identifier::try_from(name.to_string())
        .map_err(|_| unsupported!("{name} is not a valid identifier"))
}

impl<'h> Shapes<'h> {
    pub(super) fn callable(&self, value: &'h Expr) -> &'h Expr {
        match value {
            Expr::FuncRef(id) => self.lexical_functions.get(id).copied().unwrap_or(value),
            _ => value,
        }
    }

    pub(super) fn new(
        module: pliron::builtin::ops::ModuleOp,
        hir: &'h HirModule,
        foreign: &'h HashMap<String, crate::wit::Foreign<'h>>,
        metadata: &'h crate::hir_compat::Metadata<'h>,
        specialization: &'h crate::engine::Specialization,
    ) -> Self {
        let decls = Declarations::new(
            &hir.interfaces,
            &hir.type_aliases,
            &hir.imports,
            &hir.classes,
            foreign,
        );
        let mut static_fields = HashMap::new();
        let mut next_static_id = 0x8000_0000u32;
        for class in hir
            .classes
            .iter()
            .filter(|class| decls.classes.named.contains_key(class.name.as_str()))
        {
            for field in &class.static_fields {
                static_fields.insert(
                    (class.name.as_str(), field.name.as_str()),
                    StaticField {
                        id: next_static_id,
                        field,
                        ty: specialization
                            .instances
                            .class_fields
                            .get(&class.name)
                            .and_then(|layout| layout.statics.get(&field.name))
                            .copied(),
                    },
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
        if !hir.functions.is_empty() || !decls.classes.named.is_empty() {
            global_ids.extend(static_fields.values().map(|field| field.id));
        }
        Shapes {
            specialization,
            module,
            foreign_wrappers: RefCell::new(HashMap::new()),
            globals: RefCell::new(HashMap::new()),
            global_ids,
            decls,
            static_fields,
            class_records: RefCell::default(),
            resolved: RefCell::default(),
            runtime_records: RefCell::default(),
            runtime_layouts: specialization
                .instances
                .operations
                .values()
                .filter_map(|operation| match operation {
                    crate::engine::NativeOperation::Allocate(layout) => {
                        Some((layout.name.as_str(), layout))
                    }
                    _ => None,
                })
                .collect(),
            lexical_functions: perry_hir::emitted_closures(hir).into_iter().collect(),
            function_names,
            local_source_spans: &hir.local_source_spans,
            metadata,
            pending: RefCell::new(HashMap::new()),
            named: RefCell::new(HashMap::new()),
            structural_records: RefCell::default(),
            inputs: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn local_name(&self, id: perry_hir::types::LocalId) -> Option<&str> {
        let span = self.local_source_spans.get(&id)?;
        self.metadata
            .locations
            .source_at(span.start)?
            .get(..span.end.checked_sub(span.start)? as usize)
    }

    pub(super) fn annotate_local(
        &self,
        error: Diagnostic,
        id: perry_hir::types::LocalId,
    ) -> Diagnostic {
        match self.local_source_spans.get(&id) {
            Some(span) => error.with_fallback_span(
                self.metadata
                    .locations
                    .offset_span(span.start, span.end.saturating_sub(span.start)),
            ),
            None => error,
        }
    }

    pub(super) fn annotate_byte_offset(
        &self,
        error: Diagnostic,
        byte_offset: u32,
        len: u32,
    ) -> Diagnostic {
        error.with_fallback_span(self.metadata.locations.offset_span(byte_offset, len))
    }

    fn annotate_site(
        &self,
        error: Diagnostic,
        span: Option<perry_wit_diagnostics::SourceSpan>,
    ) -> Diagnostic {
        let Some(site) = span else {
            return error;
        };
        match error.span() {
            None => error.with_span(site),
            Some(inner)
                if inner.file != site.file
                    || inner.start_byte < site.start_byte
                    || inner.end_byte > site.end_byte =>
            {
                error.with_spanned_note(site, "called from here")
            }
            Some(_) => error,
        }
    }

    pub(super) fn annotate_call(&self, error: Diagnostic, byte_offset: u32) -> Diagnostic {
        self.annotate_site(error, self.metadata.locations.call_span(byte_offset))
    }

    pub(super) fn annotate_member(&self, error: Diagnostic, byte_offset: u32) -> Diagnostic {
        self.annotate_site(error, self.metadata.locations.member_span(byte_offset))
    }

    pub(super) fn has_function(&self, name: &str) -> bool {
        self.function_names.contains(name)
    }

    pub(super) fn foreign_function(
        &self,
        local: &str,
    ) -> Option<(&'h str, &'h crate::wit::ImportSignature<'h>)> {
        self.decls
            .foreign_functions
            .get(local)
            .copied()
            .or_else(|| {
                let (binding, crate::wit::Foreign::Function(sig)) =
                    self.decls.foreign.get_key_value(local)?
                else {
                    return None;
                };
                Some((binding, sig))
            })
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
                .map_err(|_| Diagnostic::internal("opaque input record"))?
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
                .to_diag(ctx)?
                .into()
        } else {
            ty
        };
        self.inputs.borrow_mut().insert(ty, mapped);
        Ok(mapped)
    }

    pub(super) fn deferred_binding(&self, id: perry_hir::types::LocalId) -> bool {
        self.local_source_spans
            .get(&id)
            .is_some_and(|span| self.metadata.source.binding(span.start).deferred)
    }

    pub(super) fn deferred_storage(
        &self,
        ctx: &mut Context,
        id: perry_hir::types::LocalId,
        ty: TypeHandle,
    ) -> Option<TypeHandle> {
        (self.deferred_binding(id)
            && !perry_mir::admits_undefined(ctx, ty)
            && !ty.deref(ctx).is::<perry_mir::JsonType>())
        .then(|| crate::lower::coerce::admitting(ctx, ty, perry_mir::Absence::Undefined))
    }

    pub(super) fn binding_annotation(
        &self,
        id: perry_hir::types::LocalId,
    ) -> Option<BindingAnnotation> {
        let span = self.local_source_spans.get(&id)?;
        Some(
            if self.metadata.source.binding(span.start).inferred(span.end) {
                BindingAnnotation::Inferred
            } else {
                BindingAnnotation::Explicit
            },
        )
    }

    /// Whether the program declares a type of this name (shadowing a built-in class).
    pub(super) fn is_user_type(&self, name: &str) -> bool {
        self.decls.is_user_type(name)
    }

    pub(super) fn class_of_type(&self, ctx: &Context, ty: TypeHandle) -> Option<&'h str> {
        let inner = crate::lower::present_type(ctx, ty);
        self.class_records.borrow().get(&inner).copied()
    }

    pub(super) fn static_field(
        &self,
        class_name: &str,
        field_name: &str,
    ) -> Option<perry_hir::types::LocalId> {
        self.static_fields
            .get(&(class_name, field_name))
            .map(|field| field.id)
    }

    fn class_member(
        &self,
        class_name: &str,
        find: impl FnOnce(&'h perry_hir::ir::Class) -> Option<&'h perry_hir::ir::Function>,
    ) -> Option<(&'h str, &'h perry_hir::ir::Function)> {
        let (&owner_name, &(_, class)) = self.decls.classes.named.get_key_value(class_name)?;
        Some((owner_name, find(class)?))
    }

    pub(super) fn class_method(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<(&'h str, &'h perry_hir::ir::Function)> {
        let norm = crate::hir_compat::method(method_name).name();
        self.class_member(class_name, |c| c.methods.iter().find(|m| m.name == norm))
    }

    pub(super) fn class_static_method(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<(&'h str, &'h perry_hir::ir::Function)> {
        let norm = crate::hir_compat::method(method_name).name();
        self.class_member(class_name, |c| {
            c.static_methods.iter().find(|m| m.name == norm)
        })
    }

    pub(super) fn class_getter(
        &self,
        class_name: &str,
        prop_name: &str,
    ) -> Option<(&'h str, &'h perry_hir::ir::Function)> {
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
    ) -> Option<(&'h str, &'h perry_hir::ir::Function)> {
        self.class_member(class_name, |c| {
            c.setters
                .iter()
                .find(|(n, _)| n == prop_name)
                .map(|(_, f)| f)
        })
    }

    pub(super) fn named(&self, ctx: &Context, name: &str) -> Result<TypeHandle> {
        self.resolve_named(ctx, name)
            .map_err(|error| error.with_fallback_span(self.metadata.locations.type_span(name)))
    }

    pub(super) fn storage_binding(
        &self,
        ctx: &Context,
        storage: crate::engine::Storage,
    ) -> Result<BindingType> {
        use crate::engine::{Storage, types::Ty};
        let ty = match storage {
            Storage::Value(ty) => return self.resolved_binding(ctx, ty),
            Storage::Promise(ty) => PromiseType::get(
                ctx,
                self.resolved_binding(ctx, if ty == Ty::BOTTOM { Ty::VOID } else { ty })?
                    .carrier,
            )
            .into(),
            Storage::AbortController => perry_mir::AbortType::get(ctx).into(),
            Storage::Boxed { ty, init } => {
                self.box_layout(ctx, ty, init.admits_undefined())?.0.into()
            }
        };
        Ok(BindingType::plain(ty))
    }

    pub(super) fn box_layout(
        &self,
        ctx: &Context,
        ty: Ty,
        deferred: bool,
    ) -> Result<(TypedHandle<RecordType>, TypeHandle)> {
        let binding = self.resolved_binding(ctx, ty)?;
        let value = if deferred {
            crate::lower::coerce::admitting(ctx, binding.carrier, perry_mir::Absence::Undefined)
        } else {
            binding.expected_type(ctx)
        };
        Ok((self.mir_record(ctx, vec![("v".into(), value)])?, value))
    }

    fn resolve_named(&self, ctx: &Context, name: &str) -> Result<TypeHandle> {
        if let Some(layout) = self.runtime_layouts.get(name) {
            if let Some(record) = self.runtime_records.borrow().get(name) {
                return Ok((*record).into());
            }
            let record = fresh_record(ctx, RecordKind::Object, None)?;
            self.runtime_records
                .borrow_mut()
                .insert(layout.name.as_str(), record);
            let mut fields = layout
                .fields
                .iter()
                .map(|(field, ty)| {
                    let ty = self.storage_binding(ctx, *ty)?.expected_type(ctx);
                    Ok((field.clone(), ty))
                })
                .collect::<Result<Vec<_>>>()?;
            fields.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let name = record.deref(ctx).name().clone();
            return Ok(RecordType::get(ctx, name, Some(fields))
                .to_diag(ctx)?
                .into());
        }
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
        bail_diag!(codes::UNSUPPORTED_TYPE, "type {name} is not supported yet")
    }

    /// Reserve recursive identity before resolving source fields.
    fn source_record(
        &self,
        ctx: &Context,
        named: Option<&InstanceKey>,
        mut fields: Properties,
    ) -> Result<TypedHandle<RecordType>> {
        fields.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if let Some(record) = self.structural_records.borrow().get(&fields) {
            return Ok(*record);
        }
        if let Some(named) = named {
            let reentry = self.pending.borrow().get(named).copied();
            match reentry {
                Some(PendingShape::Recursive(reserved)) => return Ok(reserved),
                Some(PendingShape::Resolving) => {
                    let reserved = fresh_record(ctx, RecordKind::Object, None)?;
                    self.structural_records
                        .borrow_mut()
                        .insert(fields, reserved);
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
                let binding = self
                    .resolved_binding(ctx, *ty)
                    .map_err(|e| e.with_note(format!("in field '{field}'")))?;
                let ty = if *optional {
                    binding.carrier
                } else {
                    binding.expected_type(ctx)
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
        let typed = typed.inspect_err(|_| {
            self.structural_records.borrow_mut().remove(&fields);
        })?;
        let record = match reserved {
            Some(record) => {
                let name = record.deref(ctx).name().clone();
                RecordType::get(ctx, name, Some(typed)).to_diag(ctx)
            }
            None if let Some(record) = self.structural_records.borrow().get(&fields) => Ok(*record),
            None => self.mir_record(ctx, typed),
        }?;
        self.structural_records.borrow_mut().insert(fields, record);
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
        RecordType::structural(ctx, kind, fields).to_diag(ctx)
    }

    /// Field names, in source order, of a synthesized object-literal shape.
    pub(super) fn literal_fields(&self, class_name: &str) -> Option<&[&'h str]> {
        self.decls.classes.literal_fields(class_name)
    }
}
