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
    ExceptionType, FsStatsType, JsonType, OptionalType, PromiseType, RecordField, RecordType,
    StringType, TextDecoderType, TextEncoderType, TextOrBytesType, TupleType,
};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::{TypeHandle, TypedHandle},
};
use std::{cell::RefCell, collections::HashMap};

/// Source spans distinguish inferred bindings from explicit typed boundaries after HIR lowering.
pub(super) fn inferred_binding_spans(
    module: &perry_parser::swc_ecma_ast::Module,
) -> std::collections::HashSet<(u32, u32)> {
    use perry_parser::swc_ecma_ast as ast;
    use swc_ecma_visit::{Visit, VisitWith};
    #[derive(Default)]
    struct InferredBindings(std::collections::HashSet<(u32, u32)>);
    impl Visit for InferredBindings {
        fn visit_var_declarator(&mut self, declaration: &ast::VarDeclarator) {
            if let ast::Pat::Ident(binding) = &declaration.name
                && binding.type_ann.is_none()
                && declaration.init.is_some()
            {
                self.0.insert((binding.id.span.lo.0, binding.id.span.hi.0));
            }
            declaration.visit_children_with(self);
        }
    }
    let mut bindings = InferredBindings::default();
    module.visit_with(&mut bindings);
    bindings.0
}

pub(crate) struct Shapes<'h> {
    hir: &'h HirModule,
    pub(crate) inferred_bindings: std::collections::HashSet<perry_hir::types::LocalId>,
    /// Named types whose fields are being computed, with the record name reserved when a
    /// field refers back to the type.
    pending: RefCell<HashMap<String, Option<String>>>,
    /// Record names reserved for recursive types.
    reserved: RefCell<usize>,
    /// Resolved named types, so a recursive interface keeps its reserved name.
    named: RefCell<HashMap<String, TypeHandle>>,
    dictionaries: RefCell<HashMap<String, PendingDictionary>>,
    resolving_tuples: RefCell<std::collections::HashSet<String>>,
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
    pub(crate) fn new(
        hir: &'h HirModule,
        inferred_bindings: std::collections::HashSet<perry_hir::types::LocalId>,
    ) -> Self {
        Shapes {
            hir,
            inferred_bindings,
            pending: RefCell::new(HashMap::new()),
            reserved: RefCell::new(0),
            named: RefCell::new(HashMap::new()),
            dictionaries: RefCell::new(HashMap::new()),
            resolving_tuples: RefCell::new(std::collections::HashSet::new()),
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
                let mut value = self.value_type(ctx, first)?;
                for ty in present {
                    let other = self.value_type(ctx, ty)?;
                    if value != other {
                        ensure!(
                            [value, other].into_iter().all(|ty| {
                                let ty = ty.deref(ctx);
                                ty.is::<StringType>()
                                    || ty.is::<BytesType>()
                                    || ty.is::<TextOrBytesType>()
                            }),
                            "unions of these different shapes are not supported yet"
                        );
                        value = TextOrBytesType::get(ctx).into();
                    }
                }
                if types.iter().any(|ty| matches!(ty, HirType::Void)) {
                    OptionalType::get(ctx, value).into()
                } else {
                    value
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
            HirType::Tuple(elements) => {
                let elements = elements
                    .iter()
                    .map(|ty| self.value_type(ctx, ty))
                    .collect::<Result<Vec<_>>>()?;
                TupleType::get(ctx, elements).into()
            }
            HirType::Array(element) => {
                let element = self.value_type(ctx, element)?;
                ArrayType::get(ctx, element).into()
            }
            HirType::Named(name) if name == super::source::FS_STATS => FsStatsType::get(ctx).into(),
            HirType::Named(name) if name == super::source::ENCODER => {
                TextEncoderType::get(ctx).into()
            }
            HirType::Named(name) if name == super::source::DECODER => {
                TextDecoderType::get(ctx).into()
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
                .map(|p| (p.name.as_str(), &p.ty, p.optional))
                .collect();
            return Ok(self.hir_record(ctx, Some(name), fields)?.into());
        }
        if let Some(alias) = self.hir.type_aliases.iter().find(|a| a.name == name) {
            ensure!(
                alias.type_params.is_empty(),
                "generic type alias {name} is not supported yet"
            );
            return match &alias.ty {
                HirType::Object(object) => self.object(ctx, Some(name), object),
                HirType::Tuple(_) => {
                    ensure!(
                        self.resolving_tuples.borrow_mut().insert(name.to_owned()),
                        "recursive tuple aliases are not supported yet"
                    );
                    let result = self.value_type(ctx, &alias.ty);
                    self.resolving_tuples.borrow_mut().remove(name);
                    result
                }
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
            .map(|(name, info)| (name.as_str(), &info.ty, info.optional))
            .collect();
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
                let mut ty = self
                    .value_type(ctx, ty)
                    .with_context(|| format!("field {field}"))?;
                if *optional && !ty.deref(ctx).is::<OptionalType>() {
                    ty = OptionalType::get(ctx, ty).into();
                }
                Ok(RecordField {
                    name: identifier(field)?,
                    ty,
                    optional: *optional,
                })
            })
            .collect::<Result<Vec<_>>>();
        let reserved = named.and_then(|named| self.pending.borrow_mut().remove(named).flatten());
        let typed = typed?;
        match reserved {
            Some(name) => RecordType::get(ctx, identifier(&name)?, Some(typed))
                .map_err(|e| anyhow!("{}", e.disp(ctx))),
            None => self.record_schema(ctx, typed),
        }
    }

    /// A record from MIR field types, in any order.
    pub(crate) fn mir_record(
        &self,
        ctx: &mut Context,
        fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<RecordType>> {
        let fields = fields
            .into_iter()
            .map(|(name, ty)| {
                Ok(RecordField {
                    name: identifier(&name)?,
                    ty,
                    optional: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.record_schema(ctx, fields)
    }

    fn record_schema(
        &self,
        ctx: &mut Context,
        mut fields: Vec<RecordField>,
    ) -> Result<TypedHandle<RecordType>> {
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = fields.iter().map(|field| field.name.to_string()).collect();
        let base = record_name(&names.iter().map(String::as_str).collect::<Vec<_>>());
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
