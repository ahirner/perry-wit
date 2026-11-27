//! Map frontend declarations and record identities to MIR representations.
//!
//! Frontend record identities map to carriers; object construction evaluates fields in source order.
use crate::lower::{
    Diagnostic, LocalBinding, Result, bail_diag, codes, diag, ensure_diag, ensure_unsupported,
    missing_type_annotation, unsupported,
};
use perry_hir::types::Type as HirType;
use perry_mir::{
    ArrayType, OptionalType, PlironResultExt as _, PromiseType, RecordKind, RecordType, StringType,
};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::{TypeHandle, TypedHandle},
};
use std::collections::{BTreeMap, HashMap};

mod collection_types;
use crate::engine::types::{LocId, ObjectProp, PropertiesId, Ty, TyKind};
use crate::shapes::{Declarations, generics::InstanceKey};

mod generics;
mod representation;
mod resolved;
mod unions;

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

    fn integer(ctx: &Context, domain: TypedHandle<IntegerType>) -> Self {
        let carrier = if domain.deref(ctx).width() == 64 {
            perry_mir::BigIntType::get(ctx).into()
        } else {
            FP64Type::get(ctx).into()
        };
        Self {
            carrier,
            integer: Some(domain),
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ShapeKey {
    Application(InstanceKey),
    Structural(PropertiesId),
}

pub(super) struct Prepared<'h> {
    pub(super) source: &'h crate::engine::types::TypeTable,
    bindings: crate::engine::types::IdxVec<Ty, Result<BindingType>>,
}

pub(super) struct Preparing<'h> {
    source: &'h mut crate::engine::types::TypeTable,
    wit_input: bool,
    resolved: HashMap<(Ty, bool), BindingType>,
    timer: Option<TypeHandle>,
    depth: usize,
    projections: HashMap<(ShapeKey, bool), Option<TypeHandle>>,
}

pub(super) struct Shapes<'h, T = Prepared<'h>> {
    pub(super) specialization: &'h crate::engine::Specialization<'h>,
    pub(super) records: perry_mir::RecordLayouts,
    pub(super) types: T,
    pub(super) module: pliron::builtin::ops::ModuleOp,
    pub(super) foreign_wrappers: HashMap<(&'h str, Vec<TypeHandle>), Identifier>,
    pub(super) globals: BTreeMap<perry_hir::types::LocalId, Option<LocalBinding<TypeHandle>>>,
    pub(super) decls: &'h Declarations<'h>,
    callables: crate::engine::BitSet,
    pub(super) class_records: HashMap<TypeHandle, crate::shapes::ClassShape<'h>>,
    pub(super) source: &'h crate::source::ModuleDeclarations,
    pub(super) locations: &'h crate::hir_compat::Locations<'h>,
}

pub(super) fn identifier(name: &str) -> Result<Identifier> {
    Identifier::try_from(name.to_string())
        .map_err(|_| unsupported!("{name} is not a valid identifier"))
}

impl<'h> Shapes<'h, Preparing<'h>> {
    pub(super) fn new(
        module: pliron::builtin::ops::ModuleOp,
        declarations: &'h crate::source::ModuleDeclarations,
        decls: &'h Declarations<'h>,
        locations: &'h crate::hir_compat::Locations<'h>,
        specialization: &'h crate::engine::Specialization<'h>,
        layouts: (
            &'h mut crate::engine::types::TypeTable,
            perry_mir::RecordLayouts,
        ),
    ) -> Self {
        let global_ids = specialization.instances.globals.iter().copied().chain(
            specialization
                .bodies
                .static_fields
                .values()
                .map(|field| field.id),
        );
        Shapes {
            records: layouts.1,
            specialization,
            types: Preparing {
                source: layouts.0,
                wit_input: false,
                resolved: HashMap::default(),
                timer: None,
                depth: 0,
                projections: HashMap::new(),
            },
            module,
            foreign_wrappers: HashMap::new(),
            globals: global_ids.map(|id| (id, None)).collect(),
            decls,
            callables: Default::default(),
            class_records: HashMap::default(),
            source: declarations,
            locations,
        }
    }

    pub(super) fn prepare(mut self, ctx: &Context) -> Shapes<'h> {
        self.types.source.intern_imports(ctx, self.decls.world);
        let mut bindings = crate::engine::types::IdxVec::default();
        for ty in self.types.source.ids() {
            let value = self.resolved_binding(ctx, ty);
            bindings.push(value);
        }
        Shapes {
            types: Prepared {
                source: self.types.source,
                bindings,
            },
            records: self.records,
            specialization: self.specialization,
            module: self.module,
            foreign_wrappers: self.foreign_wrappers,
            globals: self.globals,
            decls: self.decls,
            callables: self.callables,
            class_records: self.class_records,
            source: self.source,
            locations: self.locations,
        }
    }
}

impl<'h, T> Shapes<'h, T> {
    pub(super) fn local_name(&self, id: perry_hir::types::LocalId) -> Option<&str> {
        let span = self.source.locals.get(&id)?.span;
        self.locations
            .source_at(span.lo.0)?
            .get(..span.hi.0.checked_sub(span.lo.0)? as usize)
    }

    pub(super) fn annotate_local(
        &self,
        error: Diagnostic,
        id: perry_hir::types::LocalId,
    ) -> Diagnostic {
        match self.source.locals.get(&id) {
            Some(local) => error.with_fallback_span(self.locations.span(local.span)),
            None => error,
        }
    }

    pub(super) fn annotate_byte_offset(
        &self,
        error: Diagnostic,
        byte_offset: u32,
        len: u32,
    ) -> Diagnostic {
        error.with_fallback_span(self.locations.offset_span(byte_offset, len))
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
        self.annotate_site(error, self.locations.call_span(byte_offset))
    }

    pub(super) fn annotate_member(&self, error: Diagnostic, byte_offset: u32) -> Diagnostic {
        self.annotate_site(error, self.locations.member_span(byte_offset))
    }

    pub(super) fn has_function(&self, name: &str) -> bool {
        self.specialization
            .bodies
            .functions
            .iter()
            .any(|f| f.name == name)
    }
}

impl<'h, T> Shapes<'h, T> {
    pub(super) fn deferred_storage(
        &self,
        ctx: &mut Context,
        id: perry_hir::types::LocalId,
        ty: TypeHandle,
    ) -> Option<TypeHandle> {
        (self
            .specialization
            .instances
            .binding_flags(id)
            .contains(crate::engine::types::LocalFlags::DEFERRED)
            && !perry_mir::admits_undefined(ctx, ty)
            && !ty.deref(ctx).is::<perry_mir::JsonType>())
        .then(|| crate::lower::coerce::admitting(ctx, ty, perry_mir::Absence::Undefined))
    }

    /// Whether the program declares a type of this name (shadowing a built-in class).
    pub(super) fn is_user_type(&self, name: &str) -> bool {
        self.decls.is_user_type(name)
    }

    pub(super) fn class_of_type(&self, ctx: &Context, ty: TypeHandle) -> Option<&'h str> {
        let inner = crate::lower::present_type(ctx, ty);
        self.class_records.get(&inner).map(|class| class.name())
    }

    pub(super) fn static_field(
        &self,
        class: &str,
        name: &str,
    ) -> Result<perry_hir::types::LocalId> {
        self.specialization
            .bodies
            .static_fields
            .get(&(class, name))
            .map(|field| field.id)
            .ok_or_else(|| {
                diag!(
                    codes::TYPE_MISMATCH,
                    "static field {class}.{name} not found"
                )
            })
    }

    pub(super) fn instance_class(
        &self,
        name: &str,
    ) -> Option<&'h crate::source::InstanceClass<'h>> {
        match *self.decls.classes.get(name)? {
            crate::shapes::ClassShape::Instance(class) => Some(class),
            crate::shapes::ClassShape::Source(_) => None,
        }
    }
}

impl Shapes<'_, Preparing<'_>> {
    pub(super) fn named(&mut self, ctx: &Context, name: &str) -> Result<TypeHandle> {
        self.resolve_named(ctx, name)
            .map_err(|error| error.with_fallback_span(self.locations.type_span(name)))
    }
}

impl<'h> Shapes<'h> {
    pub(super) fn resolved_binding(&self, ty: Ty) -> Result<BindingType> {
        self.types.bindings[ty].clone()
    }

    pub(super) fn storage_binding(
        &mut self,
        ctx: &Context,
        storage: crate::engine::Storage,
    ) -> Result<BindingType> {
        use crate::engine::{Storage, types::Ty};
        let ty = match storage {
            Storage::ErasedFunction(_) => {
                return Err(Diagnostic::internal("static function has no storage"));
            }
            Storage::Value(ty) => return self.resolved_binding(ty),
            Storage::Promise(ty) => PromiseType::get(
                ctx,
                self.resolved_binding(if ty == Ty::BOTTOM { Ty::VOID } else { ty })?
                    .carrier,
            )
            .into(),
            Storage::Boxed { ty, init } => {
                self.box_layout(ctx, ty, init.admits_undefined())?.0.into()
            }
        };
        Ok(BindingType::plain(ty))
    }

    pub(super) fn box_layout(
        &mut self,
        ctx: &Context,
        ty: Ty,
        deferred: bool,
    ) -> Result<(TypedHandle<RecordType>, TypeHandle)> {
        let binding = self.resolved_binding(ty)?;
        let value = if deferred {
            crate::lower::coerce::admitting(ctx, binding.carrier, perry_mir::Absence::Undefined)
        } else {
            binding.expected_type(ctx)
        };
        Ok((self.mir_record(ctx, vec![("v".into(), value)])?, value))
    }
}

impl<'h, T> Shapes<'h, T> {
    pub(super) fn environment(
        &self,
        identity: crate::engine::types::FuncIdentity,
    ) -> Result<&'h crate::engine::Environment> {
        self.specialization
            .instances
            .environments
            .get(&identity)
            .ok_or_else(|| Diagnostic::internal("missing closure environment layout"))
    }
}

impl Shapes<'_> {
    pub(super) fn define_environments(&mut self, ctx: &Context) -> Result<()> {
        let plans = &self.specialization.instances;
        for layout in plans.environments.values() {
            let record = layout.record;
            let fields = plans.environment_fields[layout.fields]
                .iter()
                .enumerate()
                .map(|(slot, field)| {
                    let name = match field {
                        crate::engine::EnvironmentField::Unit => "_u".to_owned(),
                        _ => format!("c{slot}"),
                    };
                    Ok((
                        name,
                        self.storage_binding(ctx, field.storage())?
                            .expected_type(ctx),
                    ))
                })
                .collect::<Result<_>>()?;
            let name = record.deref(ctx).name().clone();
            RecordType::get(ctx, name, Some(fields)).to_diag(ctx)?;
        }
        Ok(())
    }
}

impl<'h> Shapes<'h, Preparing<'_>> {
    fn resolve_named(&mut self, ctx: &Context, name: &str) -> Result<TypeHandle> {
        // Built-in classes the subset models (a user type or explicit import takes precedence).
        let user = self.is_user_type(name);
        if !user {
            if let Some(entry) = self.decls.imported_types.get(name) {
                return self.foreign_value_type(ctx, entry);
            }
            if let Some(ty) = perry_mir::builtin_value_type(ctx, name) {
                return Ok(ty);
            }
            if self.decls.world.function(name).is_some() {
                bail_diag!(
                    codes::TYPE_MISMATCH,
                    "foreign function cannot be used as a value type"
                );
            }
            if let Some(entry) = self.decls.world.foreign().get(name) {
                return self.foreign_value_type(ctx, entry);
            }
        }
        if user {
            return self.generic(ctx, name, &[]);
        }
        bail_diag!(codes::UNSUPPORTED_TYPE, "type {name} is not supported yet")
    }

    fn record_fields(
        &mut self,
        ctx: &Context,
        properties: &[ObjectProp],
        visiting: &mut Vec<LocId>,
    ) -> Result<Vec<(String, TypeHandle)>> {
        properties
            .iter()
            .map(|p| {
                let name = self.types.source.name_str(p.name).to_owned();
                let field = self
                    .project_type(ctx, p.ty, visiting)
                    .map_err(|e| e.with_note(format!("in field '{name}'")))?;
                let carrier = if p.optional {
                    OptionalType::get(ctx, field.carrier).into()
                } else {
                    field.expected_type(ctx)
                };
                Ok((name, carrier))
            })
            .collect()
    }

    fn declared_record(
        &mut self,
        ctx: &Context,
        key: InstanceKey,
        props: PropertiesId,
        class: Option<crate::shapes::ClassShape<'h>>,
    ) -> Result<TypeHandle> {
        let mut fields = self.types.source.object_slice(props).to_vec();
        let identity = if let Some(class) = class {
            if !matches!(class, crate::shapes::ClassShape::Instance(c) if c.direct_constructor) {
                for field in &mut fields {
                    field.optional = true;
                }
            }
            ShapeKey::Application(key)
        } else {
            for field in &mut fields {
                field.readonly = false;
            }
            fields.sort_unstable_by_key(|p| self.types.source.name_str(p.name));
            ShapeKey::Structural(self.types.source.intern_properties(&fields))
        };
        let identity = (identity, self.types.wit_input);
        if let Some(slot) = self.types.projections.get_mut(&identity) {
            if let Some(record) = *slot {
                return Ok(record);
            }
            let record = self
                .records
                .fresh(ctx, RecordKind::Object, None)
                .to_diag(ctx)?;
            return Ok(*slot.insert(record.into()));
        }
        self.types.projections.insert(identity, None);
        let fields = self.record_fields(ctx, &fields, &mut Vec::new());
        let recursive = self.types.projections.remove(&identity).flatten();
        let fields = fields?;
        let record = match recursive {
            Some(record) => {
                let name = perry_mir::typed::<RecordType>(ctx, record)
                    .expect("reserved record")
                    .deref(ctx)
                    .name()
                    .clone();
                RecordType::get(ctx, name, Some(fields)).to_diag(ctx)?
            }
            None if class.is_some() => self
                .records
                .fresh(ctx, RecordKind::Object, Some(fields))
                .to_diag(ctx)?,
            None => self.mir_record(ctx, fields)?,
        };
        self.types.projections.insert(identity, Some(record.into()));
        if !self.types.wit_input
            && let Some(class) = class
        {
            self.class_records.insert(record.into(), class);
        }
        Ok(record.into())
    }
}

impl<'h, T> Shapes<'h, T> {
    /// A record from MIR field types, in any order.
    pub(super) fn mir_record(
        &mut self,
        ctx: &Context,
        fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        self.records
            .structural(ctx, RecordKind::Object, fields)
            .to_diag(ctx)
    }

    pub(super) fn positional_record(
        &mut self,
        ctx: &Context,
        kind: RecordKind,
        types: impl IntoIterator<Item = TypeHandle>,
    ) -> Result<TypedHandle<RecordType>> {
        let fields = types
            .into_iter()
            .enumerate()
            .map(|(i, ty)| (format!("_{i}"), ty))
            .collect();
        self.records.structural(ctx, kind, fields).to_diag(ctx)
    }

    /// Field names, in source order, of a synthesized object-literal shape.
    pub(super) fn literal_fields(
        &self,
        class_name: &str,
    ) -> Option<&'h [perry_hir::ir::ClassField]> {
        self.specialization.literal_classes.get(class_name).copied()
    }
}
