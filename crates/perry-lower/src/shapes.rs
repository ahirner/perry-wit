//! Map frontend declarations and record identities to MIR representations.
//!
//! The frontend canonicalizes structural fields. Nominal classes and recursive declarations
//! reserve fresh MIR record handles, and object construction evaluates fields in source order.
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
use perry_frontend::shapes::{Declarations, RecordIdentity, captured};

mod generics;
mod representation;
use perry_frontend::shapes::perry_integer_spec;
use representation::integer_mir_carrier;

pliron::dict_key!(MIR_RECORD_INTERNER, "perry_mir_record_interner");
pliron::context_registration!(register_mir_record_interner);
type RecordKey = (RecordKind, RecordIdentity<TypeHandle>);
struct RecordInterner {
    records: std::sync::Mutex<HashMap<RecordKey, TypedHandle<RecordType>>>,
    next_record: std::sync::atomic::AtomicUsize,
}
fn register_mir_record_interner(ctx: &mut Context) {
    let idx = ctx.aux_data.insert(Box::new(RecordInterner {
        records: std::sync::Mutex::new(HashMap::new()),
        next_record: std::sync::atomic::AtomicUsize::new(0),
    }));
    ctx.aux_data_map.insert(MIR_RECORD_INTERNER.clone(), idx);
}
fn record_interner(ctx: &Context) -> &RecordInterner {
    let idx = ctx.aux_data_map[&MIR_RECORD_INTERNER];
    ctx.aux_data[idx]
        .downcast_ref::<RecordInterner>()
        .expect("record interner registered on Context")
}
fn fresh_record(
    ctx: &Context,
    kind: RecordKind,
    fields: Option<Vec<(String, TypeHandle)>>,
) -> Result<TypedHandle<RecordType>> {
    let interner = record_interner(ctx);
    loop {
        let id = interner
            .next_record
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let name = identifier(&format!("shape_{id}"))?;
        let Ok(record) = RecordType::get_kind(ctx, name.clone(), None, kind) else {
            continue;
        };
        if record.deref(ctx).fields().is_ok() {
            continue;
        }
        return match fields {
            Some(fields) => RecordType::get_kind(ctx, name, Some(fields), kind)
                .map_err(|e| anyhow!("{}", e.disp(ctx))),
            None => Ok(record),
        };
    }
}

type InstanceKey = (perry_frontend::shapes::generics::DeclarationId, Vec<TypeId>);

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
    pub(super) classes: HashMap<&'h str, (usize, &'h perry_frontend::hir::ir::Class)>,
    pub(super) static_fields: HashMap<
        (&'h str, &'h str),
        (
            perry_frontend::hir::types::LocalId,
            &'h perry_frontend::hir::ir::ClassField,
        ),
    >,
    pub(super) class_records: RefCell<HashMap<TypeHandle, (&'h str, Vec<TypeId>)>>,
    object_literal_shapes: HashMap<&'h str, Vec<&'h str>>,
    function_names: HashSet<&'h str>,
    local_source_spans:
        &'h HashMap<perry_frontend::hir::types::LocalId, perry_frontend::hir::ir::LocalSourceSpan>,
    pub(super) metadata: &'h crate::hir_compat::Metadata<'h>,
    /// Named types whose fields are being computed, with a fresh record handle reserved when a
    /// field refers back to the type.
    pending: RefCell<HashMap<InstanceKey, PendingShape>>,
    /// Resolved nominal instances keyed by declaration identity and type arguments.
    named: RefCell<HashMap<InstanceKey, TypeHandle>>,
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
        let mut classes = HashMap::new();
        let mut static_fields = HashMap::new();
        let mut next_static_id = 0x8000_0000u32;
        let object_literal_shapes = crate::hir_compat::object_literal_shapes(hir);
        for (idx, class) in hir.classes.iter().enumerate() {
            if !crate::hir_compat::is_object_literal(&class.name) {
                classes.insert(class.name.as_str(), (idx, class));
                for field in &class.static_fields {
                    static_fields.insert(
                        (class.name.as_str(), field.name.as_str()),
                        (next_static_id, field),
                    );
                    next_static_id += 1;
                }
            }
        }
        let function_names = hir
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        let mut global_ids = captured(hir);
        if !hir.functions.is_empty() || !classes.is_empty() {
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
            object_literal_shapes,
            function_names,
            local_source_spans: &hir.local_source_spans,
            metadata,
            pending: RefCell::new(HashMap::new()),
            named: RefCell::new(HashMap::new()),
            inputs: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn annotate_local(
        &self,
        error: anyhow::Error,
        id: perry_frontend::hir::types::LocalId,
    ) -> anyhow::Error {
        match self.local_source_spans.get(&id) {
            Some(span) => self.metadata.locations.annotate_offset(
                error,
                span.start,
                span.end.saturating_sub(span.start),
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
        self.metadata
            .locations
            .annotate_offset(error, byte_offset, len)
    }

    pub(super) fn annotate_call(&self, error: anyhow::Error, byte_offset: u32) -> anyhow::Error {
        self.metadata.locations.annotate_call(error, byte_offset)
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
        let (&owner_name, &(_, class)) = self.classes.get_key_value(class_name)?;
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
        self.resolve_named(ctx, name)
            .map_err(|error| self.metadata.locations.annotate_type(error, name))
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
            if let Some(ty) = perry_mir::ValueKind::builtin_type(ctx, name) {
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
        nominal: bool,
        fields: Vec<(&str, &HirType, bool)>,
    ) -> Result<TypedHandle<RecordType>> {
        let identity = RecordIdentity::named(
            fields
                .into_iter()
                .map(|(name, ty, optional)| (name, (ty, optional)))
                .collect(),
        );
        let fields: Vec<_> = identity
            .fields()
            .iter()
            .map(|(name, (ty, optional))| (*name, *ty, *optional))
            .collect();
        if let Some(named) = named {
            let reentry = self.pending.borrow().get(named).copied();
            match reentry {
                Some(PendingShape::Recursive(reserved)) => return Ok(reserved),
                Some(PendingShape::Resolving) => {
                    let reserved = fresh_record(ctx, RecordKind::Object, None)?;
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
        match reserved {
            Some(record) => {
                let name = record.deref(ctx).name().clone();
                RecordType::get(ctx, name, Some(typed)).map_err(|e| anyhow!("{}", e.disp(ctx)))
            }
            None if nominal => fresh_record(ctx, RecordKind::Object, Some(typed)),
            None => self.mir_record(ctx, typed),
        }
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
        let identity = if kind == RecordKind::Object {
            RecordIdentity::named(fields)
        } else {
            RecordIdentity::ordered(fields)
        };
        let key = (kind, identity);
        let interner = record_interner(ctx);
        if let Some(record) = interner
            .records
            .lock()
            .expect("record interner lock poisoned")
            .get(&key)
        {
            return Ok(*record);
        }
        let record = fresh_record(ctx, kind, Some(key.1.fields().to_vec()))?;
        interner
            .records
            .lock()
            .expect("record interner lock poisoned")
            .insert(key, record);
        Ok(record)
    }

    /// Field names, in source order, of a synthesized object-literal shape.
    pub(super) fn literal_fields(&self, class_name: &str) -> Option<&[&'h str]> {
        self.object_literal_shapes
            .get(class_name)
            .map(Vec::as_slice)
    }
}
