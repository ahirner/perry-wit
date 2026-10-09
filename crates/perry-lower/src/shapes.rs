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
use perry_mir::types::{ArrayType, RecordType, StringType};
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

impl<'h> Shapes<'h> {
    pub(crate) fn new(hir: &'h HirModule) -> Self {
        Shapes {
            hir,
            pending: RefCell::new(HashMap::new()),
            reserved: RefCell::new(0),
            named: RefCell::new(HashMap::new()),
        }
    }

    /// MIR type of a HIR value type, for the types supported so far.
    pub(crate) fn value_type(&self, ctx: &mut Context, ty: &HirType) -> Result<TypeHandle> {
        Ok(match ty {
            HirType::Number | HirType::Int32 => FP64Type::get(ctx).into(),
            HirType::Boolean => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            HirType::String | HirType::StringLiteral(_) => StringType::get(ctx).into(),
            HirType::Array(element) => {
                let element = self.value_type(ctx, element)?;
                ArrayType::get(ctx, element).into()
            }
            HirType::Named(name) => self.named(ctx, name)?,
            HirType::Object(object) => {
                ensure!(
                    object.index_signature.is_none(),
                    "index signatures need the dictionary type (subset rule R1), which is not supported yet"
                );
                let fields = object
                    .properties
                    .iter()
                    .map(|(name, info)| {
                        ensure!(!info.optional, "optional field {name} is not supported yet");
                        Ok((name.as_str(), &info.ty))
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.hir_record(ctx, None, fields)?.into()
            }
            other => bail!("MIR backend does not support type {other:?} yet"),
        })
    }

    fn named(&self, ctx: &mut Context, name: &str) -> Result<TypeHandle> {
        if let Some(ty) = self.named.borrow().get(name) {
            return Ok(*ty);
        }
        let ty = self.resolve_named(ctx, name)?;
        // A re-entrant reference returns the opaque record; only the outermost call caches.
        if !self.pending.borrow().contains_key(name) {
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
            return self.value_type(ctx, &alias.ty);
        }
        bail!("type {name} is not supported by the MIR backend yet")
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
