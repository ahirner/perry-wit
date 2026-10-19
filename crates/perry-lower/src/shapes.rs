//! HIR types to MIR types, with records identified by structure.
//!
//! TypeScript object types are structural, so a record is interned by its [`RecordKind`] and sorted
//! `(field_name, field_type)` pairs: an interface `P { x; y }`, a type literal `{ y; x }`, and
//! Perry's synthesized `__AnonShape_*` class for `{ y: …, x: … }` all resolve to the same
//! `perry.record` handle. Nominal classes and recursive declarations allocate fresh record handles.
//! Field order inside an object record is the sorted order; evaluation order stays the source order.
//! Widening a record to a narrower shape (subset rule R2) is not supported yet.
//!
//! WIT types come from the SDK's generated declarations, which the parser does not read: the
//! compiler passes them as *foreign* HIR types under their SDK names (prefixed `wit:`), and imports of those
//! names resolve to them. Finite variants retain their case-specific payload types.
//! Options distinguish admitted null and undefined states; tuples use `{ _0: A, _1: B }`.
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_hir::{ir::Module as HirModule, types::Type as HirType};
use perry_mir::{ArrayType, OptionalType, PromiseType, RecordKind, RecordType, StringType};
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

mod collection_types;
mod globals;
pub(crate) use globals::{class_functions, class_functions_mut};
pub(super) mod generics;
mod representation;
pub(crate) use representation::{integer_hir_carrier, integer_mir_carrier, perry_integer_spec};

#[derive(Clone, Debug, PartialEq)]
struct TypeKey(HirType);

impl Eq for TypeKey {}

impl std::hash::Hash for TypeKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        hash_hir_type(&self.0, state);
    }
}

pliron::dict_key!(HIR_TYPE_INTERNER, "perry_hir_type_interner");
pliron::context_registration!(register_hir_type_interner);

type RecordStructure = (RecordKind, Vec<(String, TypeHandle)>);

struct TypeInterner {
    hir_types: std::sync::Mutex<HashMap<u64, Vec<TypeId>>>,
    records: std::sync::Mutex<HashMap<RecordStructure, TypedHandle<RecordType>>>,
    next_record: std::sync::atomic::AtomicUsize,
}

fn register_hir_type_interner(ctx: &mut Context) {
    let idx = ctx.aux_data.insert(Box::new(TypeInterner {
        hir_types: std::sync::Mutex::new(HashMap::new()),
        records: std::sync::Mutex::new(HashMap::new()),
        next_record: std::sync::atomic::AtomicUsize::new(0),
    }));
    ctx.aux_data_map.insert(HIR_TYPE_INTERNER.clone(), idx);
}

fn hir_type_hash(ty: &HirType) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_hir_type(ty, &mut hasher);
    hasher.finish()
}

/// An interned [`HirType`] handle backed by [`Context`]'s [`pliron::uniqued_any::UniqueStore`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct TypeId(pliron::uniqued_any::Uniqued<TypeKey>);

impl TypeId {
    fn interner(ctx: &Context) -> &TypeInterner {
        let idx = ctx.aux_data_map[&HIR_TYPE_INTERNER];
        ctx.aux_data[idx]
            .downcast_ref::<TypeInterner>()
            .expect("type interner registered on Context")
    }

    fn fresh_record(
        ctx: &Context,
        kind: RecordKind,
        fields: Option<Vec<(String, TypeHandle)>>,
    ) -> Result<TypedHandle<RecordType>> {
        let interner = Self::interner(ctx);
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

    fn lookup(ctx: &Context, interner: &TypeInterner, hash: u64, ty: &HirType) -> Option<Self> {
        let map = interner.hir_types.lock().expect("type interner lock");
        map.get(&hash)?.iter().copied().find(|id| id.hir(ctx) == ty)
    }

    fn insert_miss(ctx: &Context, interner: &TypeInterner, hash: u64, ty: HirType) -> Self {
        let id = Self(pliron::uniqued_any::Uniqued::new(ctx, TypeKey(ty)));
        interner
            .hir_types
            .lock()
            .expect("type interner lock")
            .entry(hash)
            .or_default()
            .push(id);
        id
    }

    pub(crate) fn intern(ctx: &Context, ty: &HirType) -> Self {
        let interner = Self::interner(ctx);
        let hash = hir_type_hash(ty);
        Self::lookup(ctx, interner, hash, ty)
            .unwrap_or_else(|| Self::insert_miss(ctx, interner, hash, ty.clone()))
    }

    pub(crate) fn from_owned(ctx: &Context, ty: HirType) -> Self {
        let interner = Self::interner(ctx);
        let hash = hir_type_hash(&ty);
        if let Some(id) = Self::lookup(ctx, interner, hash, &ty) {
            return id;
        }
        Self::insert_miss(ctx, interner, hash, ty)
    }

    pub(crate) fn hir(self, ctx: &Context) -> &HirType {
        &self.0.get(ctx).0
    }

    pub(crate) fn intern_slice(ctx: &Context, types: &[HirType]) -> Vec<Self> {
        if types.is_empty() {
            return Vec::new();
        }
        let interner = Self::interner(ctx);
        types
            .iter()
            .map(|ty| {
                let hash = hir_type_hash(ty);
                Self::lookup(ctx, interner, hash, ty)
                    .unwrap_or_else(|| Self::insert_miss(ctx, interner, hash, ty.clone()))
            })
            .collect()
    }
}

fn hash_hir_type<H: std::hash::Hasher>(ty: &HirType, state: &mut H) {
    use std::hash::{Hash as _, Hasher as _};
    std::mem::discriminant(ty).hash(state);
    match ty {
        HirType::StringLiteral(s) | HirType::Named(s) | HirType::TypeVar(s) => s.hash(state),
        HirType::Array(inner) | HirType::Promise(inner) => hash_hir_type(inner, state),
        HirType::Tuple(items) | HirType::Union(items) => {
            items.len().hash(state);
            for item in items {
                hash_hir_type(item, state);
            }
        }
        HirType::Object(obj) => {
            obj.name.hash(state);
            obj.property_order.hash(state);
            obj.properties.len().hash(state);
            let mut sum = 0u64;
            let mut xor = 0u64;
            for (k, v) in &obj.properties {
                let mut entry = std::collections::hash_map::DefaultHasher::new();
                k.hash(&mut entry);
                v.optional.hash(&mut entry);
                v.readonly.hash(&mut entry);
                hash_hir_type(&v.ty, &mut entry);
                let h = entry.finish();
                sum = sum.wrapping_add(h);
                xor ^= h;
            }
            sum.hash(state);
            xor.hash(state);
            obj.index_signature.is_some().hash(state);
            if let Some(idx) = &obj.index_signature {
                hash_hir_type(idx, state);
            }
        }
        HirType::Function(f) => {
            f.is_async.hash(state);
            f.is_generator.hash(state);
            f.params.len().hash(state);
            for (name, pty, opt) in &f.params {
                name.hash(state);
                opt.hash(state);
                hash_hir_type(pty, state);
            }
            hash_hir_type(&f.return_type, state);
        }
        HirType::Generic { base, type_args } => {
            base.hash(state);
            type_args.len().hash(state);
            for arg in type_args {
                hash_hir_type(arg, state);
            }
        }
        HirType::Any
        | HirType::Unknown
        | HirType::Void
        | HirType::Never
        | HirType::Null
        | HirType::Boolean
        | HirType::Number
        | HirType::Int32
        | HirType::BigInt
        | HirType::String
        | HirType::Symbol => {}
    }
}

type InstanceKey = (generics::DeclarationId, Vec<TypeId>);

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

pub(crate) struct Declarations<'h> {
    pub(super) interfaces: HashMap<&'h str, (usize, &'h perry_hir::ir::Interface)>,
    pub(super) type_aliases: HashMap<&'h str, (usize, &'h perry_hir::ir::TypeAlias)>,
    class_names: HashSet<String>,
    pub(super) foreign: &'h HashMap<String, crate::hir::Foreign>,
    pub(super) integer_imports: HashMap<&'h str, (u32, Signedness)>,
    pub(super) imported_types: HashMap<&'h str, &'h crate::hir::Foreign>,
    pub(super) foreign_functions: HashMap<&'h str, (String, &'h perry_hir::types::FunctionType)>,
}

impl<'h> Declarations<'h> {
    pub(crate) fn new(
        interfaces: &'h [perry_hir::ir::Interface],
        type_aliases: &'h [perry_hir::ir::TypeAlias],
        imports: &'h [perry_hir::ir::Import],
        classes: &[perry_hir::ir::Class],
        foreign: &'h HashMap<String, crate::hir::Foreign>,
    ) -> Self {
        let interfaces = interfaces
            .iter()
            .enumerate()
            .map(|(idx, interface)| (interface.name.as_str(), (idx, interface)))
            .collect();
        let type_aliases = type_aliases
            .iter()
            .enumerate()
            .map(|(idx, alias)| (alias.name.as_str(), (idx, alias)))
            .collect();
        let class_names = classes
            .iter()
            .filter(|c| !crate::hir_compat::is_object_literal(&c.name))
            .map(|c| c.name.clone())
            .collect();
        let mut integer_imports = HashMap::new();
        let mut imported_types = HashMap::new();
        let mut foreign_functions = HashMap::new();
        for import in imports {
            let source = import
                .source
                .strip_prefix("wit:")
                .unwrap_or(import.source.as_str());
            for specifier in &import.specifiers {
                let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                    continue;
                };
                if import.source == "perry:types"
                    && let Some(spec) = perry_integer_spec(imported)
                {
                    integer_imports.insert(local.as_str(), spec);
                    continue;
                }
                let binding = format!("{source}#{imported}");
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
        Self {
            interfaces,
            type_aliases,
            class_names,
            foreign,
            integer_imports,
            imported_types,
            foreign_functions,
        }
    }

    pub(crate) fn is_user_type(&self, name: &str) -> bool {
        self.interfaces.contains_key(name)
            || self.type_aliases.contains_key(name)
            || self.class_names.contains(name)
    }

    pub(crate) fn integer_spec(&self, name: &str) -> Option<(u32, Signedness)> {
        let mut seen = HashSet::new();
        self.integer_spec_by_name(name, &mut seen)
    }

    pub(crate) fn integer_carrier(&self, name: &str) -> Option<HirType> {
        self.integer_spec(name)
            .map(|(width, _)| integer_hir_carrier(width))
    }

    fn integer_spec_by_name<'a>(
        &'a self,
        name: &'a str,
        seen: &mut HashSet<&'a str>,
    ) -> Option<(u32, Signedness)> {
        if let Some(spec) = name
            .strip_prefix("perry:types/")
            .and_then(perry_integer_spec)
        {
            return Some(spec);
        }
        if !seen.insert(name) {
            return None;
        }
        if !self.is_user_type(name) {
            if let Some(&spec) = self.integer_imports.get(name) {
                return Some(spec);
            }
            if let Some(entry) = self
                .imported_types
                .get(name)
                .copied()
                .or_else(|| self.foreign.get(name))
            {
                return self.foreign_integer_spec(entry, seen);
            }
        }
        if let Some((_, alias)) = self
            .type_aliases
            .get(name)
            .copied()
            .filter(|(_, a)| a.type_params.is_empty())
            && let HirType::Named(target) = &alias.ty
        {
            return self.integer_spec_by_name(target, seen);
        }
        None
    }

    fn foreign_integer_spec<'a>(
        &'a self,
        entry: &'a crate::hir::Foreign,
        seen: &mut HashSet<&'a str>,
    ) -> Option<(u32, Signedness)> {
        match entry {
            crate::hir::Foreign::Integer { width, signedness } => Some((*width, *signedness)),
            crate::hir::Foreign::Type(HirType::Named(name)) => {
                self.integer_spec_by_name(name, seen)
            }
            crate::hir::Foreign::Input(inner) => self.foreign_integer_spec(inner, seen),
            crate::hir::Foreign::Type(_)
            | crate::hir::Foreign::Resource(_)
            | crate::hir::Foreign::Function(_) => None,
        }
    }

    pub(crate) fn resolve_foreign_type(&self, type_name: &str) -> Option<&'h HirType> {
        if self.is_user_type(type_name) {
            return None;
        }
        let mut entry = self
            .imported_types
            .get(type_name)
            .copied()
            .or_else(|| self.foreign.get(type_name))?;
        for _ in 0..8 {
            match entry {
                crate::hir::Foreign::Type(HirType::Named(next)) => {
                    entry = self.foreign.get(next.as_str())?;
                }
                crate::hir::Foreign::Type(ty) => return Some(ty),
                _ => return None,
            }
        }
        None
    }
}

pub(super) struct Shapes<'h> {
    pub(super) module: pliron::builtin::ops::ModuleOp,
    pub(super) foreign_wrappers: RefCell<HashMap<(String, Vec<TypeHandle>), Identifier>>,
    pub(super) globals: RefCell<HashMap<perry_hir::types::LocalId, BindingType>>,
    pub(super) global_ids: HashSet<perry_hir::types::LocalId>,
    pub(super) type_env: perry_hir::HirTypeEnv,
    pub(super) decls: Declarations<'h>,
    pub(super) classes: HashMap<&'h str, (usize, &'h perry_hir::ir::Class)>,
    pub(super) static_fields:
        HashMap<(&'h str, &'h str), (perry_hir::types::LocalId, &'h perry_hir::ir::ClassField)>,
    pub(super) class_records: RefCell<HashMap<TypeHandle, (&'h str, Vec<TypeId>)>>,
    object_literal_shapes: HashMap<&'h str, Vec<&'h str>>,
    function_names: HashSet<&'h str>,
    local_source_spans: &'h HashMap<perry_hir::types::LocalId, perry_hir::ir::LocalSourceSpan>,
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
        let mut object_literal_shapes = HashMap::new();
        for (idx, class) in hir.classes.iter().enumerate() {
            if crate::hir_compat::is_object_literal(&class.name) {
                object_literal_shapes.insert(
                    class.name.as_str(),
                    class.fields.iter().map(|f| f.name.as_str()).collect(),
                );
            } else {
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
        let mut global_ids = globals::captured(hir);
        if !hir.functions.is_empty() || !classes.is_empty() {
            global_ids.extend(static_fields.values().map(|(id, _)| *id));
        }
        Shapes {
            module,
            foreign_wrappers: RefCell::new(HashMap::new()),
            globals: RefCell::new(HashMap::new()),
            global_ids,
            type_env: perry_hir::HirTypeEnv::from_module(hir),
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
        id: perry_hir::types::LocalId,
    ) -> anyhow::Error {
        match self.local_source_spans.get(&id) {
            Some(span) => self.metadata.locations.annotate(
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
        self.metadata.locations.annotate(
            error,
            swc_common::Span::new(
                swc_common::BytePos(byte_offset),
                swc_common::BytePos(byte_offset.saturating_add(len)),
            ),
        )
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
    ) -> Option<(String, &perry_hir::types::FunctionType)> {
        self.decls
            .foreign_functions
            .get(local)
            .map(|(binding, sig)| (binding.clone(), *sig))
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
            let mapped = TypeId::fresh_record(ctx, kind, None)?;
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

    pub(super) fn is_explicit_binding(&self, id: perry_hir::types::LocalId) -> bool {
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
    ) -> Option<(perry_hir::types::LocalId, &'h perry_hir::ir::ClassField)> {
        self.static_fields.get(&(class_name, field_name)).copied()
    }

    fn class_member(
        &self,
        class_name: &str,
        find: impl FnOnce(&'h perry_hir::ir::Class) -> Option<&'h perry_hir::ir::Function>,
    ) -> Option<(&'h str, &'h perry_hir::ir::Function)> {
        let (&owner_name, &(_, class)) = self.classes.get_key_value(class_name)?;
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
        mut fields: Vec<(&str, &HirType, bool)>,
    ) -> Result<TypedHandle<RecordType>> {
        fields.sort_by(|a, b| a.0.cmp(b.0));
        if let Some(named) = named {
            let reentry = self.pending.borrow().get(named).copied();
            match reentry {
                Some(PendingShape::Recursive(reserved)) => return Ok(reserved),
                Some(PendingShape::Resolving) => {
                    let reserved = TypeId::fresh_record(ctx, RecordKind::Object, None)?;
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
            None if nominal => TypeId::fresh_record(ctx, RecordKind::Object, Some(typed)),
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
        mut fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        if kind == RecordKind::Object {
            fields.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let key = (kind, fields);
        let interner = TypeId::interner(ctx);
        if let Some(&record) = interner
            .records
            .lock()
            .expect("type interner lock poisoned")
            .get(&key)
        {
            return Ok(record);
        }
        let record = TypeId::fresh_record(ctx, kind, Some(key.1.clone()))?;
        interner
            .records
            .lock()
            .expect("type interner lock poisoned")
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

pub(crate) trait HirTypeExt {
    fn any(&self, f: &mut impl FnMut(&HirType) -> bool) -> bool;
    fn transform(&mut self, f: &mut impl FnMut(&mut HirType));
}

impl HirTypeExt for HirType {
    fn any(&self, f: &mut impl FnMut(&HirType) -> bool) -> bool {
        if f(self) {
            return true;
        }
        match self {
            HirType::Array(inner) | HirType::Promise(inner) => inner.any(f),
            HirType::Tuple(elements) | HirType::Union(elements) => {
                elements.iter().any(|item| item.any(f))
            }
            HirType::Generic { type_args, .. } => type_args.iter().any(|arg| arg.any(f)),
            HirType::Object(obj) => {
                obj.properties.values().any(|p| p.ty.any(f))
                    || obj.index_signature.as_ref().is_some_and(|idx| idx.any(f))
            }
            HirType::Function(func) => {
                func.params.iter().any(|(_, ty, _)| ty.any(f)) || func.return_type.any(f)
            }
            _ => false,
        }
    }

    fn transform(&mut self, f: &mut impl FnMut(&mut HirType)) {
        match self {
            HirType::Array(inner) | HirType::Promise(inner) => inner.transform(f),
            HirType::Tuple(elements) | HirType::Union(elements) => {
                for item in elements {
                    item.transform(f);
                }
            }
            HirType::Generic { type_args, .. } => {
                for arg in type_args {
                    arg.transform(f);
                }
            }
            HirType::Object(obj) => {
                for p in obj.properties.values_mut() {
                    p.ty.transform(f);
                }
                if let Some(idx) = &mut obj.index_signature {
                    idx.transform(f);
                }
            }
            HirType::Function(func) => {
                for (_, ty, _) in &mut func.params {
                    ty.transform(f);
                }
                func.return_type.transform(f);
            }
            _ => {}
        }
        f(self);
    }
}
