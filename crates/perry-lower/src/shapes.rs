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
use pliron::printable::Printable as _;
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::{TypeHandle, TypedHandle},
};
use std::{cell::RefCell, collections::HashMap};

pub(crate) struct Shapes<'h> {
    pub(crate) module: pliron::builtin::ops::ModuleOp,
    pub(crate) foreign_wrappers: RefCell<HashMap<(String, Vec<TypeHandle>), Identifier>>,
    pub(crate) decoders: RefCell<HashMap<TypeHandle, Identifier>>,
    pub(crate) globals: RefCell<HashMap<perry_hir::types::LocalId, TypeHandle>>,
    pub(crate) global_ids: std::collections::HashSet<perry_hir::types::LocalId>,
    pub(crate) inferred_bindings: std::collections::HashSet<perry_hir::types::LocalId>,
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
    inputs: RefCell<HashMap<TypeHandle, TypeHandle>>,
}

pub(crate) fn identifier(name: &str) -> Result<Identifier> {
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
    pub(crate) fn new(
        module: pliron::builtin::ops::ModuleOp,
        hir: &'h HirModule,
        foreign: &'h HashMap<String, HirType>,
        inferred_bindings: std::collections::HashSet<perry_hir::types::LocalId>,
    ) -> Self {
        let aliases = hir
            .imports
            .iter()
            .flat_map(|import| import.specifiers.iter().map(move |s| (&import.source, s)))
            .filter_map(|(source, specifier)| {
                let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                    return None;
                };
                [
                    format!("wit:{source}#{imported}"),
                    format!("wit:{imported}"),
                ]
                .into_iter()
                .find(|name| foreign.contains_key(name))
                .map(|name| (local.clone(), name))
            })
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
            hir,
            foreign,
            aliases,
            pending: RefCell::new(HashMap::new()),
            reserved: RefCell::new(0),
            named: RefCell::new(HashMap::new()),
            inputs: RefCell::new(HashMap::new()),
        }
    }

    pub(crate) fn has_function(&self, name: &str) -> bool {
        self.hir
            .functions
            .iter()
            .any(|function| function.name == name)
    }

    pub(crate) fn foreign_function(
        &self,
        local: &str,
    ) -> Option<(String, &perry_hir::types::FunctionType)> {
        self.hir.imports.iter().find_map(|import| {
            import.specifiers.iter().find_map(|specifier| {
                let perry_hir::ir::ImportSpecifier::Named {
                    imported,
                    local: name,
                } = specifier
                else {
                    return None;
                };
                if name != local {
                    return None;
                }
                let binding = format!("{}#{imported}", import.source);
                let HirType::Function(signature) =
                    self.foreign.get(&format!("wit-function:{binding}"))?
                else {
                    return None;
                };
                Some((binding, signature))
            })
        })
    }

    /// MIR type of a HIR value type, for the types supported so far.
    pub(crate) fn value_type(&self, ctx: &mut Context, ty: &HirType) -> Result<TypeHandle> {
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
            HirType::Generic { base, type_args } if base == "PerryWitResource" => {
                let [HirType::StringLiteral(key)] = type_args.as_slice() else {
                    bail!("invalid WIT resource identity")
                };
                perry_mir::types::ResourceType::get(ctx, key.parse()?).into()
            }
            HirType::Generic { base, type_args } if base == "WitInput" => {
                let [inner] = type_args.as_slice() else {
                    bail!("WitInput needs one type argument");
                };
                let inner = self.value_type(ctx, inner)?;
                self.input_type(ctx, inner)?
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
            HirType::Object(object) => {
                ensure!(
                    object.index_signature.is_none(),
                    "index signatures are outside the closed-record profile"
                );
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
                if members.iter().all(|m| matches!(m, HirType::Object(o) if matches!(o.properties.get("ok").map(|p| &p.ty), Some(HirType::Boolean)))) {
                    return self.result(ctx, members);
                }
                if members.iter().any(|ty| matches!(ty, HirType::Number))
                    && members.iter().any(|ty| matches!(ty, HirType::String))
                    && members
                        .iter()
                        .all(|ty| matches!(ty, HirType::Number | HirType::String))
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
                    .map(|(i, ty)| Ok((format!("_{i}"), self.value_type(ctx, ty)?)))
                    .collect::<Result<Vec<_>>>()?;
                self.mir_record_kind(ctx, fields, true)?.into()
            }
            other => bail!("MIR backend does not support type {other:?} yet"),
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
    fn result(&self, ctx: &mut Context, members: &[HirType]) -> Result<TypeHandle> {
        ensure!(
            members.len() == 2,
            "a tagged result needs exactly two alternatives"
        );
        let mut fields = vec![(
            "ok".to_string(),
            IntegerType::get(ctx, 1, Signedness::Signless).into(),
        )];
        for name in ["value", "error"] {
            let mut payload = None;
            for member in members {
                let HirType::Object(object) = member else {
                    unreachable!()
                };
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
                "AbortController" | "AbortSignal" => {
                    return Ok(perry_mir::types::AbortType::get(ctx).into());
                }
                "Uint8Array" => return Ok(perry_mir::types::BytesType::get(ctx).into()),
                "URL" => return Ok(perry_mir::types::UrlType::get(ctx).into()),
                "URLSearchParams" => return Ok(perry_mir::types::SearchParamsType::get(ctx).into()),
                "Error" | "TypeError" | "RangeError" => {
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
                .map(|p| (p.name.as_str(), &p.ty, p.optional))
                .collect();
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
        mut fields: Vec<(&str, &HirType, bool)>,
    ) -> Result<TypedHandle<RecordType>> {
        fields.sort_by(|a, b| a.0.cmp(b.0));
        let names: Vec<&str> = fields.iter().map(|(name, _, _)| *name).collect();
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
            .map(|(field, ty, optional)| {
                let ty = self
                    .value_type(ctx, ty)
                    .with_context(|| format!("field {field}"))?;
                let ty = if *optional {
                    OptionalType::get(ctx, ty).into()
                } else {
                    ty
                };
                Ok((field.to_string(), ty))
            })
            .collect::<Result<Vec<_>>>();
        let reserved = named.and_then(|named| self.pending.borrow_mut().remove(named).flatten());
        let typed = typed?;
        match reserved {
            Some(name) => RecordType::get(ctx, identifier(&name)?, Some(typed))
                .map_err(|e| anyhow!("{}", e.disp(ctx))),
            None => self.mir_record(ctx, typed),
        }
    }

    /// A record from MIR field types, in any order.
    pub(crate) fn mir_record(
        &self,
        ctx: &mut Context,
        fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_record_kind(ctx, fields, false)
    }

    fn mir_record_kind(
        &self,
        ctx: &mut Context,
        mut fields: Vec<(String, TypeHandle)>,
        tuple: bool,
    ) -> Result<TypedHandle<RecordType>> {
        if !tuple {
            fields.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
        let base = record_name(&names);
        let base = if tuple { format!("tuple_{base}") } else { base };
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
    pub(crate) fn literal_fields(&self, class_name: &str) -> Option<Vec<&'h str>> {
        if !class_name.starts_with("__AnonShape_") {
            return None;
        }
        let class = self.hir.classes.iter().find(|c| c.name == class_name)?;
        Some(class.fields.iter().map(|f| f.name.as_str()).collect())
    }
}
