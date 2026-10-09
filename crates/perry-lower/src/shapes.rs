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
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_hir::{ir::Module as HirModule, types::Type as HirType};
use perry_mir::types::{
    ArrayType, BufferType, BytesType, DictionaryField, DictionarySchema, DictionaryType,
    ExceptionType, JsonType, OptionalType, PromiseType, RecordType, StringType,
};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::{TypeHandle, TypedHandle},
};
use std::{cell::RefCell, collections::HashMap};

pub(crate) struct Shapes<'h> {
    hir: &'h HirModule,
    /// Named types whose fields are being computed, with the record name reserved when a
    /// field refers back to the type.
    pending: RefCell<HashMap<String, Option<String>>>,
    /// Record names reserved for recursive types.
    reserved: RefCell<usize>,
    /// Resolved named types, so a recursive interface keeps its reserved name.
    named: RefCell<HashMap<String, TypeHandle>>,
    dictionaries: RefCell<HashMap<String, PendingDictionary>>,
}
#[derive(Clone, Copy)]
enum PendingDictionary {
    Resolving,
    Reserved(TypedHandle<DictionaryType>),
}

pub(crate) fn identifier(name: &str) -> Result<Identifier> {
    Identifier::try_from(name.to_string()).map_err(|_| anyhow!("{name} is not a MIR identifier"))
}

fn record_name(fields: &[&str]) -> String {
    if fields.is_empty() {
        "shape_".to_string()
    } else {
        format!("shape_{}", fields.join("_"))
    }
}

fn json_alias(name: &str, ty: &HirType) -> bool {
    let HirType::Union(types) = ty else {
        return false;
    };
    let recursive = |ty: &HirType| matches!(ty, HirType::Named(reference) if reference == name);
    let mut variants = 0u8;
    for ty in types {
        variants |= match ty {
            HirType::Null => 1,
            HirType::Boolean => 2,
            HirType::Number => 4,
            HirType::String => 8,
            HirType::Array(element) if recursive(element) => 16,
            HirType::Object(object)
                if object.properties.is_empty()
                    && object.index_signature.as_deref().is_some_and(recursive) =>
            {
                32
            }
            _ => return false,
        };
    }
    variants == 63 && types.len() == 6
}

impl<'h> Shapes<'h> {
    pub(crate) fn new(hir: &'h HirModule) -> Self {
        Shapes {
            hir,
            pending: RefCell::new(HashMap::new()),
            reserved: RefCell::new(0),
            named: RefCell::new(HashMap::new()),
            dictionaries: RefCell::new(HashMap::new()),
        }
    }

    /// MIR type of a HIR value type, for the types supported so far.
    pub(crate) fn value_type(&self, ctx: &mut Context, ty: &HirType) -> Result<TypeHandle> {
        if self.hir.type_aliases.iter().any(|alias| {
            (&alias.ty == ty || matches!(ty, HirType::Named(name) if name == &alias.name))
                && json_alias(&alias.name, &alias.ty)
        }) {
            return Ok(JsonType::get(ctx).into());
        }
        if matches!(ty, HirType::Object(_) | HirType::Generic { .. })
            && let Some(alias) = self.hir.type_aliases.iter().find(|alias| &alias.ty == ty)
        {
            return self.named(ctx, &alias.name);
        }
        Ok(match ty {
            HirType::Union(types) => {
                let mut present = types.iter().filter(|ty| !matches!(ty, HirType::Void));
                let first = present
                    .next()
                    .context("undefined-only union has no value type")?;
                let first = self.value_type(ctx, first)?;
                for ty in present {
                    ensure!(
                        self.value_type(ctx, ty)? == first,
                        "unions of different shapes are not supported yet"
                    );
                }
                if types.iter().any(|ty| matches!(ty, HirType::Void)) {
                    OptionalType::get(ctx, first).into()
                } else {
                    first
                }
            }
            HirType::Number | HirType::Int32 => FP64Type::get(ctx).into(),
            HirType::Boolean => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            HirType::String | HirType::StringLiteral(_) => StringType::get(ctx).into(),
            HirType::Generic { base, type_args } if base == "Record" => {
                ensure!(
                    type_args.len() == 2 && matches!(type_args[0], HirType::String),
                    "Record dictionaries require string keys (subset rule R1)"
                );
                let element = self.value_type(ctx, &type_args[1])?;
                DictionaryType::get(ctx, element).into()
            }
            HirType::Array(element) => {
                let element = self.value_type(ctx, element)?;
                ArrayType::get(ctx, element).into()
            }
            HirType::Named(name) if name == "Uint8Array" => BytesType::get(ctx).into(),
            HirType::Named(name) if name == "ArrayBuffer" => BufferType::get(ctx).into(),
            HirType::Named(name)
                if matches!(
                    name.as_str(),
                    "Error" | "TypeError" | "RangeError" | "SyntaxError"
                ) =>
            {
                ExceptionType::get(ctx).into()
            }
            HirType::Named(name) => self.named(ctx, name)?,
            HirType::Promise(inner) => {
                let settled = self.settled_type(ctx, inner)?;
                PromiseType::get(ctx, settled).into()
            }
            HirType::Object(object) => self.object(ctx, None, object)?,
            other => bail!("MIR backend does not support type {other:?} yet"),
        })
    }

    /// The value a promise of `ty` settles with; `void` settles `undefined`.
    pub(crate) fn settled_type(&self, ctx: &mut Context, ty: &HirType) -> Result<TypeHandle> {
        match ty {
            HirType::Void => {
                let number = FP64Type::get(ctx).into();
                Ok(OptionalType::get(ctx, number).into())
            }
            ty => self.value_type(ctx, ty),
        }
    }

    fn named(&self, ctx: &mut Context, name: &str) -> Result<TypeHandle> {
        if let Some(ty) = self.named.borrow().get(name) {
            return Ok(*ty);
        }
        let ty = self.resolve_named(ctx, name)?;
        // A re-entrant reference returns the opaque record; only the outermost call caches.
        if !self.pending.borrow().contains_key(name)
            && !self.dictionaries.borrow().contains_key(name)
        {
            self.named.borrow_mut().insert(name.to_string(), ty);
        }
        Ok(ty)
    }

    fn resolve_named(&self, ctx: &mut Context, name: &str) -> Result<TypeHandle> {
        if let Some(interface) = self.hir.interfaces.iter().find(|i| i.name == name) {
            ensure!(
                interface.type_params.is_empty()
                    && interface.extends.is_empty()
                    && interface.methods.is_empty(),
                "interface {name}: generics, extends and methods are not supported yet"
            );
            let fields = interface
                .properties
                .iter()
                .map(|p| {
                    ensure!(
                        !p.optional,
                        "optional field {}.{} is not supported yet",
                        name,
                        p.name
                    );
                    Ok((p.name.as_str(), &p.ty))
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(self.hir_record(ctx, Some(name), fields)?.into());
        }
        if let Some(alias) = self.hir.type_aliases.iter().find(|a| a.name == name) {
            ensure!(
                alias.type_params.is_empty(),
                "generic type alias {name} is not supported yet"
            );
            return match &alias.ty {
                HirType::Object(object) => self.object(ctx, Some(name), object),
                HirType::Generic { base, type_args } if base == "Record" => {
                    ensure!(
                        type_args.len() == 2 && matches!(type_args[0], HirType::String),
                        "Record dictionaries require string keys (subset rule R1)"
                    );
                    self.dictionary(ctx, Some(name), &type_args[1], &HashMap::new())
                }
                ty => self.value_type(ctx, ty),
            };
        }
        bail!("type {name} is not supported by the MIR backend yet")
    }

    fn object(
        &self,
        ctx: &mut Context,
        name: Option<&str>,
        object: &perry_hir::types::ObjectType,
    ) -> Result<TypeHandle> {
        if let Some(element) = &object.index_signature {
            return self.dictionary(ctx, name, element, &object.properties);
        }
        let fields = object
            .properties
            .iter()
            .map(|(name, info)| {
                ensure!(!info.optional, "optional field {name} is not supported yet");
                Ok((name.as_str(), &info.ty))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(self.hir_record(ctx, name, fields)?.into())
    }

    fn dictionary(
        &self,
        ctx: &mut Context,
        name: Option<&str>,
        element: &HirType,
        properties: &HashMap<String, perry_hir::types::PropertyInfo>,
    ) -> Result<TypeHandle> {
        if let Some(name) = name {
            let pending = self.dictionaries.borrow().get(name).copied();
            match pending {
                Some(PendingDictionary::Reserved(ty)) => return Ok(ty.into()),
                Some(PendingDictionary::Resolving) => {
                    let reserved = DictionaryType::named(
                        ctx,
                        identifier(&format!("dictionary_{name}"))?,
                        None,
                    )
                    .map_err(|error| anyhow!("{}", error.disp(ctx)))?;
                    self.dictionaries
                        .borrow_mut()
                        .insert(name.to_owned(), PendingDictionary::Reserved(reserved));
                    return Ok(reserved.into());
                }
                None => {
                    self.dictionaries
                        .borrow_mut()
                        .insert(name.to_owned(), PendingDictionary::Resolving);
                }
            }
        }
        let schema = (|| -> Result<DictionarySchema> {
            let element = self.value_type(ctx, element)?;
            let mut fields = Vec::with_capacity(properties.len());
            for (name, info) in properties {
                let mut ty = self.value_type(ctx, &info.ty)?;
                if info.optional && !ty.deref(ctx).is::<OptionalType>() {
                    ty = OptionalType::get(ctx, ty).into();
                }
                fields.push(DictionaryField {
                    name: name.clone(),
                    ty,
                    optional: info.optional,
                });
            }
            fields.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(DictionarySchema { element, fields })
        })();
        let pending = name.and_then(|name| self.dictionaries.borrow_mut().remove(name));
        let schema = schema?;
        let ty = if matches!(pending, Some(PendingDictionary::Reserved(_))) {
            DictionaryType::named(
                ctx,
                identifier(&format!("dictionary_{}", name.unwrap()))?,
                Some(schema),
            )
            .map_err(|error| anyhow!("{}", error.disp(ctx)))?
        } else {
            DictionaryType::constrained(ctx, schema)
        };
        let dictionary = ty.deref(ctx);
        let schema = dictionary.schema().expect("resolved dictionary schema");
        for field in &schema.fields {
            ensure!(
                perry_mir::dictionaries::accepts(ctx, schema.element, field.ty),
                "dictionary field {} is incompatible with its index signature",
                field.name
            );
        }
        Ok(ty.into())
    }

    /// A record from HIR field types; `named` is the interface they come from, if any.
    fn hir_record(
        &self,
        ctx: &mut Context,
        named: Option<&str>,
        mut fields: Vec<(&str, &HirType)>,
    ) -> Result<TypedHandle<RecordType>> {
        fields.sort_by(|a, b| a.0.cmp(b.0));
        let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
        if let Some(named) = named {
            let reentry = self.pending.borrow().get(named).cloned();
            match reentry {
                Some(Some(reserved)) => {
                    return RecordType::get(ctx, identifier(&reserved)?, None)
                        .map_err(|e| anyhow!("{}", e.disp(ctx)));
                }
                Some(None) => {
                    let mut count = self.reserved.borrow_mut();
                    *count += 1;
                    let reserved = format!("{}_rec{}", record_name(&names), *count);
                    self.pending
                        .borrow_mut()
                        .insert(named.to_string(), Some(reserved.clone()));
                    return RecordType::get(ctx, identifier(&reserved)?, None)
                        .map_err(|e| anyhow!("{}", e.disp(ctx)));
                }
                None => {
                    self.pending.borrow_mut().insert(named.to_string(), None);
                }
            }
        }
        let typed = fields
            .iter()
            .map(|(field, ty)| {
                Ok((
                    field.to_string(),
                    self.value_type(ctx, ty)
                        .with_context(|| format!("field {field}"))?,
                ))
            })
            .collect::<Result<Vec<_>>>();
        let reserved = named.and_then(|named| self.pending.borrow_mut().remove(named).flatten());
        let typed = typed?;
        match reserved {
            Some(name) => {
                let fields = typed
                    .into_iter()
                    .map(|(field, ty)| Ok((identifier(&field)?, ty)))
                    .collect::<Result<Vec<_>>>()?;
                RecordType::get(ctx, identifier(&name)?, Some(fields))
                    .map_err(|e| anyhow!("{}", e.disp(ctx)))
            }
            None => self.mir_record(ctx, typed),
        }
    }

    /// A record from MIR field types, in any order.
    pub(crate) fn mir_record(
        &self,
        ctx: &mut Context,
        mut fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<String> = fields.iter().map(|(name, _)| name.clone()).collect();
        let base = record_name(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let fields = fields
            .into_iter()
            .map(|(field, ty)| Ok((identifier(&field)?, ty)))
            .collect::<Result<Vec<_>>>()?;
        // The first name whose definition matches, or which is still free.
        for suffix in 1.. {
            let name = if suffix == 1 {
                base.clone()
            } else {
                format!("{base}_{suffix}")
            };
            if let Ok(record) = RecordType::get(ctx, identifier(&name)?, Some(fields.clone())) {
                return Ok(record);
            }
        }
        unreachable!()
    }

    /// Field names, in source order, of a synthesized object-literal shape.
    pub(crate) fn literal_fields(&self, class_name: &str) -> Option<Vec<String>> {
        if !class_name.starts_with("__AnonShape_") {
            return None;
        }
        let class = self.hir.classes.iter().find(|c| c.name == class_name)?;
        Some(class.fields.iter().map(|f| f.name.clone()).collect())
    }
}

use pliron::printable::Printable as _;
