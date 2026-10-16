//! Source schemas to the existing MIR representation contracts.
use super::*;
use crate::collections::CollectionViewKind;

impl Shapes<'_> {
    /// Compatibility entry for HIR consumers; representation uses interned source identities.
    pub(crate) fn value_type(&self, ctx: &Context, ty: &HirType) -> Result<TypeHandle> {
        let mut remaining = usize::MAX;
        let source = Argument::from_hir(ctx, ty, &HashMap::new(), &mut remaining).map_err(|e| {
            if matches!(ty, HirType::TypeVar(_)) {
                PerryDiagnostic::error(
                    DiagnosticCode::UnsupportedType,
                    format!("MIR backend does not support type {ty:?} yet"),
                )
                .into()
            } else {
                e
            }
        })?;
        self.source_type(ctx, source)
    }

    pub(super) fn source_type(&self, ctx: &Context, source: ArgumentId) -> Result<TypeHandle> {
        let source = source.deref(ctx);
        match source.kind() {
            Kind::BigInt => Ok(perry_mir::BigIntType::get(ctx).into()),
            Kind::Number | Kind::Int32 => Ok(FP64Type::get(ctx).into()),
            Kind::Boolean => Ok(IntegerType::get(ctx, 1, Signedness::Signless).into()),
            Kind::String | Kind::StringLiteral(_) => Ok(StringType::get(ctx).into()),
            Kind::Null => Ok(OptionalType::get_with(
                ctx,
                FP64Type::get(ctx).into(),
                perry_mir::Absence::Null,
            )
            .into()),
            Kind::Void => Ok(OptionalType::get_with(
                ctx,
                FP64Type::get(ctx).into(),
                perry_mir::Absence::Undefined,
            )
            .into()),
            Kind::Array(element) => {
                let element_ty = self.source_type(ctx, *element)?;
                Ok(ArrayType::get(ctx, element_ty).into())
            }
            Kind::Named(name) => self.named(ctx, name),
            Kind::Promise(inner) => {
                let settled = self.settled_source_type(ctx, *inner)?;
                Ok(PromiseType::get(ctx, settled).into())
            }
            Kind::Any | Kind::Unknown => Err(PerryDiagnostic::error(
                DiagnosticCode::MissingTypeAnnotation,
                "the value type cannot be inferred for the MIR backend",
            )
            .with_help(
                "Add a concrete type annotation to this binding, parameter, or function return.",
            )
            .into()),
            Kind::Generic { base, arguments } => {
                if base == "WitInput" {
                    let [inner] = arguments.as_slice() else {
                        bail!("WitInput needs one type argument");
                    };
                    let inner = self.source_type(ctx, *inner)?;
                    return self.input_type(ctx, inner);
                }
                if matches!(base.as_str(), "Map" | "ReadonlyMap") && !self.is_user_type(base) {
                    let [k, v] = arguments.as_slice() else {
                        bail!("{base} needs key and value type arguments");
                    };
                    return self.map_source_type(ctx, *k, *v);
                }
                if let Some(view) = CollectionViewKind::from_type_name(base)
                    && !self.is_user_type(base)
                {
                    return self.collection_view_source_type(ctx, view, arguments);
                }
                if base == "Record" && !self.is_user_type(base) {
                    let [k, v] = arguments.as_slice() else {
                        bail!("Record needs key and value type arguments");
                    };
                    return self.dict_source_type(ctx, *k, *v);
                }
                if matches!(base.as_str(), "Set" | "ReadonlySet") && !self.is_user_type(base) {
                    let [element] = arguments.as_slice() else {
                        bail!("{base} needs one element type argument");
                    };
                    return self.set_source_type(ctx, *element);
                }
                if matches!(base.as_str(), "ReadonlyArray" | "Array") && !self.is_user_type(base) {
                    let [element] = arguments.as_slice() else {
                        bail!("{base} needs one element type argument");
                    };
                    let element = self.source_type(ctx, *element)?;
                    return Ok(ArrayType::get(ctx, element).into());
                }
                if base == "ReadableStream"
                    && !self.is_user_type(base)
                    && matches!(
                        arguments.as_slice(),
                        [ty] if matches!(ty.deref(ctx).kind(), Kind::Named(n) if n == "Uint8Array")
                    )
                {
                    return Ok(perry_mir::ReadableType::get(ctx).into());
                }
                self.generic(ctx, base, arguments)
            }
            Kind::Union(union_members) => self.union_type(ctx, union_members),
            Kind::Tuple(tuple_elements) => {
                let fields = tuple_elements
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| {
                        let ty = if let Some(int_ty) = self.source_integer_type(ctx, *ty)
                            && int_ty.deref(ctx).width() == 64
                        {
                            int_ty.into()
                        } else {
                            self.source_type(ctx, *ty)?
                        };
                        Ok((format!("_{index}"), ty))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(self.mir_record_kind(ctx, fields, true)?.into())
            }
            Kind::Object {
                properties,
                index_signature,
                ..
            } => {
                if let Some(val_ty) = index_signature {
                    ensure!(
                        properties.is_empty(),
                        "index signatures are outside the closed-record profile when combined with fixed properties"
                    );
                    let str_arg = Argument::from_kind(ctx, Kind::String);
                    return self.dict_source_type(ctx, str_arg, *val_ty);
                }
                Ok(self
                    .source_record(
                        ctx,
                        None,
                        properties
                            .iter()
                            .map(|p| (p.name.as_str(), p.ty, p.optional))
                            .collect(),
                    )?
                    .into())
            }
            Kind::Function { .. } | Kind::Symbol | Kind::Never => {
                let hir_ty = source.to_hir(ctx);
                Err(PerryDiagnostic::error(
                    DiagnosticCode::UnsupportedType,
                    format!("MIR backend does not support type {hir_ty:?} yet"),
                )
                .into())
            }
        }
    }

    fn union_type(&self, ctx: &Context, members: &[ArgumentId]) -> Result<TypeHandle> {
        if members
            .iter()
            .all(|ty| matches!(ty.deref(ctx).kind(), Kind::StringLiteral(_)))
        {
            return Ok(StringType::get(ctx).into());
        }
        if members.iter().all(|ty| {
            ty.deref(ctx)
                .property("ok")
                .is_some_and(|property| matches!(property.ty.deref(ctx).kind(), Kind::Boolean))
        }) {
            return self.result(ctx, members);
        }
        if members.iter().any(|ty| {
            matches!(ty.deref(ctx).kind(), Kind::Number)
                || self
                    .source_integer_type(ctx, *ty)
                    .is_some_and(|int_ty| int_ty.deref(ctx).width() <= 32)
        }) && members
            .iter()
            .any(|ty| matches!(ty.deref(ctx).kind(), Kind::String))
            && members.iter().all(|ty| {
                matches!(ty.deref(ctx).kind(), Kind::Number | Kind::String)
                    || self
                        .source_integer_type(ctx, *ty)
                        .is_some_and(|int_ty| int_ty.deref(ctx).width() <= 32)
            })
        {
            return Ok(perry_mir::NumberStringType::get(ctx).into());
        }
        let defined: Vec<_> = members
            .iter()
            .copied()
            .filter(|ty| !matches!(ty.deref(ctx).kind(), Kind::Void | Kind::Null))
            .collect();
        if defined.len() == members.len() {
            return self.variant(ctx, members);
        }
        let inner = match defined.as_slice() {
            [defined] => self.source_type(ctx, *defined)?,
            [] => bail!("an absence-only union needs a declared value type"),
            defined => self.union_type(ctx, defined)?,
        };
        let absence = match (
            members
                .iter()
                .any(|ty| matches!(ty.deref(ctx).kind(), Kind::Null)),
            members
                .iter()
                .any(|ty| matches!(ty.deref(ctx).kind(), Kind::Void)),
        ) {
            (true, true) => perry_mir::Absence::Both,
            (true, false) => perry_mir::Absence::Null,
            _ => perry_mir::Absence::Undefined,
        };
        Ok(OptionalType::get_with(ctx, inner, absence).into())
    }

    /// SDK results retain disjoint payload slots; inactive payloads admit undefined.
    fn result(&self, ctx: &Context, alternatives: &[ArgumentId]) -> Result<TypeHandle> {
        ensure!(
            alternatives.len() == 2,
            "a tagged result needs exactly two alternatives"
        );
        let mut fields = vec![(
            "ok".to_string(),
            IntegerType::get(ctx, 1, Signedness::Signless).into(),
        )];
        for name in ["value", "error"] {
            let mut payload_arg = None;
            for object in alternatives {
                let (valid_keys, found) = {
                    let object_ref = object.deref(ctx);
                    let Kind::Object { properties, .. } = object_ref.kind() else {
                        bail!("result alternatives only contain ok, value and error");
                    };
                    let valid = properties
                        .iter()
                        .all(|p| ["ok", "value", "error"].contains(&p.name.as_str()));
                    let prop = object_ref.property(name).map(|p| p.ty);
                    (valid, prop)
                };
                ensure!(
                    valid_keys,
                    "result alternatives only contain ok, value and error"
                );
                if let Some(prop_ty) = found {
                    ensure!(
                        payload_arg.is_none(),
                        "result payload {name} appears in both alternatives"
                    );
                    payload_arg = Some(prop_ty);
                }
            }
            if let Some(prop_ty) = payload_arg {
                let ty = self.source_type(ctx, prop_ty)?;
                fields.push((name.to_string(), OptionalType::get(ctx, ty).into()));
            }
        }
        Ok(self.mir_record(ctx, fields)?.into())
    }

    fn variant(&self, ctx: &Context, members: &[ArgumentId]) -> Result<TypeHandle> {
        let mut cases = Vec::new();
        for member in members {
            let (tag, valid_props, val_arg) = {
                let member_ref = member.deref(ctx);
                let Kind::Object { properties, .. } = member_ref.kind() else {
                    bail!("unions other than tagged object types are not supported yet");
                };
                let Some(tag_prop) = member_ref.property("tag") else {
                    bail!("union members need a literal `tag`");
                };
                let tag_ty = tag_prop.ty;
                let val_arg = member_ref.property("val").map(|p| p.ty);
                let valid_props = properties
                    .iter()
                    .all(|p| p.name == "tag" || p.name == "val");
                drop(member_ref);
                let tag_ref = tag_ty.deref(ctx);
                let Kind::StringLiteral(tag) = tag_ref.kind() else {
                    bail!("union members need a literal `tag`");
                };
                (tag.clone(), valid_props, val_arg)
            };
            ensure!(valid_props, "union members have `tag` and `val` only");
            ensure!(
                !cases.iter().any(|(name, _)| name == &tag),
                "variant tags must be distinct"
            );
            let payload = val_arg
                .map(|prop_ty| self.source_type(ctx, prop_ty))
                .transpose()?;
            cases.push((tag, payload));
        }
        Ok(perry_mir::VariantType::get(ctx, cases).into())
    }

    fn settled_source_type(&self, ctx: &Context, ty: ArgumentId) -> Result<TypeHandle> {
        if matches!(ty.deref(ctx).kind(), Kind::Void) {
            let number = FP64Type::get(ctx).into();
            return Ok(OptionalType::get(ctx, number).into());
        }
        self.source_type(ctx, ty)
    }

    /// The value a promise of `ty` settles with; `void` settles `undefined`.
    pub(crate) fn settled_type(&self, ctx: &Context, ty: &HirType) -> Result<TypeHandle> {
        match ty {
            HirType::Void => {
                let number = FP64Type::get(ctx).into();
                Ok(OptionalType::get(ctx, number).into())
            }
            ty => self.value_type(ctx, ty),
        }
    }

    pub(crate) fn perry_integer_spec(name: &str) -> Option<(u32, Signedness)> {
        Some(match name {
            "perry:types/u8" => (8, Signedness::Unsigned),
            "perry:types/s8" => (8, Signedness::Signed),
            "perry:types/u16" => (16, Signedness::Unsigned),
            "perry:types/s16" => (16, Signedness::Signed),
            "perry:types/u32" => (32, Signedness::Unsigned),
            "perry:types/s32" => (32, Signedness::Signed),
            "perry:types/u64" => (64, Signedness::Unsigned),
            "perry:types/s64" => (64, Signedness::Signed),
            _ => return None,
        })
    }

    pub(crate) fn perry_integer_type(
        ctx: &Context,
        name: &str,
    ) -> Option<TypedHandle<IntegerType>> {
        let (bits, signedness) = Self::perry_integer_spec(name)?;
        Some(IntegerType::get(ctx, bits, signedness))
    }

    pub(super) fn source_integer_type(
        &self,
        ctx: &Context,
        ty: ArgumentId,
    ) -> Option<TypedHandle<IntegerType>> {
        let name = match ty.deref(ctx).kind() {
            Kind::Named(name) => name.clone(),
            _ => return None,
        };
        let mut seen = HashSet::new();
        self.integer_type_by_name(ctx, &name, &mut seen)
    }

    pub(crate) fn integer_type(
        &self,
        ctx: &Context,
        ty: &HirType,
    ) -> Option<TypedHandle<IntegerType>> {
        let HirType::Named(name) = ty else {
            return None;
        };
        let mut seen = HashSet::new();
        self.integer_type_by_name(ctx, name, &mut seen)
    }

    fn integer_type_by_name<'a>(
        &'a self,
        ctx: &Context,
        name: &'a str,
        seen: &mut HashSet<&'a str>,
    ) -> Option<TypedHandle<IntegerType>> {
        if let Some(int_ty) = Self::perry_integer_type(ctx, name) {
            return Some(int_ty);
        }
        if !seen.insert(name) {
            return None;
        }
        if !self.is_user_type(name) {
            if let Some(&(width, signedness)) = self.integer_imports.get(name) {
                return Some(IntegerType::get(ctx, width, signedness));
            }
            if let Some(entry) = self
                .imported_types
                .get(name)
                .copied()
                .or_else(|| self.foreign.get(name))
            {
                return self.foreign_integer_type(ctx, entry, seen);
            }
        }
        if let Some((_, alias)) = self
            .type_aliases
            .get(name)
            .copied()
            .filter(|(_, a)| a.type_params.is_empty())
            && let HirType::Named(target) = &alias.ty
        {
            return self.integer_type_by_name(ctx, target, seen);
        }
        None
    }

    fn foreign_integer_type<'a>(
        &'a self,
        ctx: &Context,
        entry: &'a crate::hir::Foreign,
        seen: &mut HashSet<&'a str>,
    ) -> Option<TypedHandle<IntegerType>> {
        match entry {
            crate::hir::Foreign::Integer { width, signedness } => {
                Some(IntegerType::get(ctx, *width, *signedness))
            }
            crate::hir::Foreign::Type(HirType::Named(name)) => {
                self.integer_type_by_name(ctx, name, seen)
            }
            crate::hir::Foreign::Input(inner) => self.foreign_integer_type(ctx, inner, seen),
            crate::hir::Foreign::Type(_)
            | crate::hir::Foreign::Resource(_)
            | crate::hir::Foreign::Function(_) => None,
        }
    }

    pub(super) fn foreign_value_type(
        &self,
        ctx: &Context,
        entry: &crate::hir::Foreign,
    ) -> Result<TypeHandle> {
        match entry {
            crate::hir::Foreign::Type(ty) => self.value_type(ctx, ty),
            crate::hir::Foreign::Integer { width, .. } => Ok(if *width == 64 {
                perry_mir::BigIntType::get(ctx).into()
            } else {
                FP64Type::get(ctx).into()
            }),
            crate::hir::Foreign::Resource(id) => Ok(perry_mir::ResourceType::get(ctx, *id).into()),
            crate::hir::Foreign::Input(inner) => {
                let inner = self.foreign_value_type(ctx, inner)?;
                self.input_type(ctx, inner)
            }
            crate::hir::Foreign::Function(_) => {
                bail!("foreign function cannot be used as a value type")
            }
        }
    }
}
