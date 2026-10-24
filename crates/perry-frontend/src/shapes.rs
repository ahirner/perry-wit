//! Resolved source declarations and canonical HIR type identities.
use anyhow::{Context as _, Result, anyhow, ensure};
use perry_hir::types::Type as HirType;
use pliron::{builtin::types::Signedness, context::Context};
use std::collections::{HashMap, HashSet};
pub mod generics;
mod globals;
pub use globals::{captured, class_functions, class_functions_mut};
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

struct TypeInterner {
    hir_types: std::sync::Mutex<HashMap<u64, Vec<TypeId>>>,
}

fn register_hir_type_interner(ctx: &mut Context) {
    let idx = ctx.aux_data.insert(Box::new(TypeInterner {
        hir_types: std::sync::Mutex::new(HashMap::new()),
    }));
    ctx.aux_data_map.insert(HIR_TYPE_INTERNER.clone(), idx);
}

fn hir_type_hash(ty: &HirType) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_hir_type(ty, &mut hasher);
    hasher.finish()
}

/// An interned [`HirType`] handle backed by [`Context`]'s [`pliron::uniqued_any::Uniqued`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TypeId(pliron::uniqued_any::Uniqued<TypeKey>);

impl TypeId {
    fn interner(ctx: &Context) -> &TypeInterner {
        let idx = ctx.aux_data_map[&HIR_TYPE_INTERNER];
        ctx.aux_data[idx]
            .downcast_ref::<TypeInterner>()
            .expect("type interner registered on Context")
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

    pub fn intern(ctx: &Context, ty: &HirType) -> Self {
        let interner = Self::interner(ctx);
        let hash = hir_type_hash(ty);
        Self::lookup(ctx, interner, hash, ty)
            .unwrap_or_else(|| Self::insert_miss(ctx, interner, hash, ty.clone()))
    }

    pub fn from_owned(ctx: &Context, ty: HirType) -> Self {
        let interner = Self::interner(ctx);
        let hash = hir_type_hash(&ty);
        if let Some(id) = Self::lookup(ctx, interner, hash, &ty) {
            return id;
        }
        Self::insert_miss(ctx, interner, hash, ty)
    }

    pub fn hir(self, ctx: &Context) -> &HirType {
        &self.0.get(ctx).0
    }

    pub fn intern_slice(ctx: &Context, types: &[HirType]) -> Vec<Self> {
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

pub struct Declarations<'h> {
    pub interfaces: HashMap<&'h str, (usize, &'h perry_hir::ir::Interface)>,
    pub type_aliases: HashMap<&'h str, (usize, &'h perry_hir::ir::TypeAlias)>,
    class_names: HashSet<String>,
    pub foreign: &'h HashMap<String, crate::hir::Foreign>,
    pub integer_imports: HashMap<&'h str, (u32, Signedness)>,
    pub imported_types: HashMap<&'h str, &'h crate::hir::Foreign>,
    pub foreign_functions: HashMap<&'h str, (&'h str, &'h perry_hir::types::FunctionType)>,
}

impl<'h> Declarations<'h> {
    pub fn new(
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
                if let Some((key, crate::hir::Foreign::Function(signature))) =
                    foreign.get_key_value(&binding)
                {
                    foreign_functions.insert(local.as_str(), (key.as_str(), signature));
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

    pub fn is_user_type(&self, name: &str) -> bool {
        self.interfaces.contains_key(name)
            || self.type_aliases.contains_key(name)
            || self.class_names.contains(name)
    }

    pub fn integer_spec(&self, name: &str) -> Option<(u32, Signedness)> {
        let mut seen = HashSet::new();
        self.integer_spec_by_name(name, &mut seen)
    }

    pub fn integer_carrier(&self, name: &str) -> Option<HirType> {
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

    pub fn resolve_foreign_type(&self, type_name: &str) -> Option<&'h HirType> {
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

pub fn perry_integer_spec(name: &str) -> Option<(u32, Signedness)> {
    Some(match name {
        "u8" => (8, Signedness::Unsigned),
        "s8" => (8, Signedness::Signed),
        "u16" => (16, Signedness::Unsigned),
        "s16" => (16, Signedness::Signed),
        "u32" => (32, Signedness::Unsigned),
        "s32" => (32, Signedness::Signed),
        "u64" => (64, Signedness::Unsigned),
        "s64" => (64, Signedness::Signed),
        _ => return None,
    })
}

pub fn integer_hir_carrier(width: u32) -> HirType {
    if width == 64 {
        HirType::BigInt
    } else {
        HirType::Number
    }
}

pub trait HirTypeExt {
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

/// Canonical structural source identity, independent of its eventual representation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RecordIdentity<K, Name = String>(Vec<(Name, K)>);
impl<K, Name: Ord> RecordIdentity<K, Name> {
    pub fn named(mut fields: Vec<(Name, K)>) -> Self {
        fields.sort_by(|left, right| left.0.cmp(&right.0));
        Self(fields)
    }
}
impl<K, Name> RecordIdentity<K, Name> {
    pub fn ordered(fields: Vec<(Name, K)>) -> Self {
        Self(fields)
    }
    pub fn fields(&self) -> &[(Name, K)] {
        &self.0
    }
}
