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
//! names resolve to them. A WIT variant (`{ tag: "a" } | { tag: "b"; val: T }`) is the record
//! `{ tag: string, val: T | undefined }`, an option (`T | null | undefined`) is `T | undefined`
//! (`null` and `undefined` share the absent value), and a tuple `[A, B]` is the record
//! `{ _0: A, _1: B }`.
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_hir::{ir::Module as HirModule, types::Type as HirType};
use perry_mir::types::{ArrayType, OptionalType, PromiseType, RecordType, StringType};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::{TypeHandle, TypedHandle},
};
use std::{cell::RefCell, collections::HashMap};

pub(crate) struct Shapes<'h> {
    hir: &'h HirModule,
    /// WIT types by SDK name, and the local names they are imported under.
    foreign: &'h HashMap<String, HirType>,
    aliases: HashMap<String, String>,
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
    pub(crate) fn new(hir: &'h HirModule, foreign: &'h HashMap<String, HirType>) -> Self {
        let aliases = hir
            .imports
            .iter()
            .flat_map(|import| &import.specifiers)
            .filter_map(|specifier| match specifier {
                perry_hir::ir::ImportSpecifier::Named { imported, local }
                    if foreign.contains_key(&format!("wit:{imported}")) =>
                {
                    Some((local.clone(), format!("wit:{imported}")))
                }
                _ => None,
            })
            .collect();
        Shapes {
            hir,
            foreign,
            aliases,
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
            HirType::Union(members) => {
                let defined: Vec<&HirType> = members
                    .iter()
                    .filter(|m| !matches!(m, HirType::Void | HirType::Null))
                    .collect();
                if defined.len() < members.len() {
                    // `T | undefined` (R8), `T | null` and `T | null | undefined`.
                    let [defined] = defined.as_slice() else {
                        bail!("unions of several types are not supported yet")
                    };
                    let inner = self.value_type(ctx, defined)?;
                    OptionalType::get(ctx, inner).into()
                } else {
                    self.variant(ctx, &defined)?
                }
            }
            HirType::Tuple(elements) => {
                let names: Vec<String> = (0..elements.len()).map(|i| format!("_{i}")).collect();
                let fields = names.iter().map(String::as_str).zip(elements).collect();
                self.hir_record(ctx, None, fields)?.into()
            }
            other => bail!("MIR backend does not support type {other:?} yet"),
        })
    }

    /// A tagged union of object types (a WIT variant): the record `{ tag: string }`, with
    /// `val: T | undefined` when cases carry a payload, all of one type `T`.
    fn variant(&self, ctx: &mut Context, members: &[&HirType]) -> Result<TypeHandle> {
        let mut payload: Option<&HirType> = None;
        for member in members {
            let HirType::Object(object) = member else {
                bail!("unions other than tagged object types are not supported yet")
            };
            ensure!(
                matches!(
                    object.properties.get("tag").map(|p| &p.ty),
                    Some(HirType::StringLiteral(_))
                ),
                "union members need a literal `tag`"
            );
            ensure!(
                object.properties.keys().all(|k| k == "tag" || k == "val"),
                "union members have `tag` and `val` only"
            );
            if let Some(val) = object.properties.get("val") {
                if let Some(other) = payload {
                    let (a, b) = (self.value_type(ctx, other)?, self.value_type(ctx, &val.ty)?);
                    ensure!(
                        a == b,
                        "variant payloads of different types are not supported yet"
                    );
                }
                payload = Some(&val.ty);
            }
        }
        let tag = HirType::String;
        let val = payload.map(|ty| HirType::Union(vec![ty.clone(), HirType::Void]));
        let mut fields = vec![("tag", &tag)];
        if let Some(val) = &val {
            fields.push(("val", val));
        }
        Ok(self.hir_record(ctx, None, fields)?.into())
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

    /// Whether the program declares a type of this name (shadowing a built-in class).
    pub(crate) fn is_user_type(&self, name: &str) -> bool {
        self.hir.interfaces.iter().any(|i| i.name == name)
            || self.hir.type_aliases.iter().any(|a| a.name == name)
            || self.hir.classes.iter().any(|c| c.name == name)
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
        // Built-in classes the subset models (a user type of the same name takes precedence).
        let user = self.hir.interfaces.iter().any(|i| i.name == name)
            || self.hir.type_aliases.iter().any(|a| a.name == name);
        if !user {
            let key = self.aliases.get(name).map_or(name, String::as_str);
            if let Some(foreign) = self.foreign.get(key) {
                return self.value_type(ctx, foreign);
            }
            match name {
                "Uint8Array" => return Ok(perry_mir::types::BytesType::get(ctx).into()),
                "Date" => return Ok(perry_mir::types::DateType::get(ctx).into()),
                "Headers" => return Ok(perry_mir::types::HeadersType::get(ctx).into()),
                "Response" => return Ok(perry_mir::types::ResponseType::get(ctx).into()),
                "Request" => return Ok(perry_mir::types::RequestType::get(ctx).into()),
                // Byte streams: `ReadableStream<Uint8Array>` and its default reader.
                "ReadableStream" => return Ok(perry_mir::types::ReadableType::get(ctx).into()),
                "ReadableStreamDefaultReader" => {
                    return Ok(perry_mir::types::ReaderType::get(ctx).into());
                }
                // A decoder is its `fatal` flag (UTF-8 only, BOM stripped).
                "TextDecoder" => {
                    return Ok(pliron::builtin::types::IntegerType::get(
                        ctx,
                        1,
                        pliron::builtin::types::Signedness::Signless,
                    )
                    .into());
                }
                _ => {}
            }
        }
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
