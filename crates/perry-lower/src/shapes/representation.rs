//! Source schemas to the existing MIR representation contracts.
use super::*;
use crate::collections::CollectionViewKind;
use perry_frontend::shapes::generics::Resolver;

pub(super) fn integer_mir_carrier(ctx: &Context, width: u32) -> TypeHandle {
    if width == 64 {
        perry_mir::BigIntType::get(ctx).into()
    } else {
        FP64Type::get(ctx).into()
    }
}

impl Shapes<'_> {
    pub(crate) fn value_type(&self, ctx: &Context, ty: &HirType) -> Result<TypeHandle> {
        match ty {
            HirType::BigInt => Ok(perry_mir::BigIntType::get(ctx).into()),
            HirType::Number | HirType::Int32 => Ok(FP64Type::get(ctx).into()),
            HirType::Boolean => Ok(IntegerType::get(ctx, 1, Signedness::Signless).into()),
            HirType::String | HirType::StringLiteral(_) => Ok(StringType::get(ctx).into()),
            HirType::Null => Ok(OptionalType::get_with(
                ctx,
                FP64Type::get(ctx).into(),
                perry_mir::Absence::Null,
            )
            .into()),
            HirType::Void | HirType::Never => Ok(OptionalType::get_with(
                ctx,
                FP64Type::get(ctx).into(),
                perry_mir::Absence::Undefined,
            )
            .into()),
            HirType::Array(element) => {
                let element_ty = self.value_type(ctx, element)?;
                Ok(ArrayType::get(ctx, element_ty).into())
            }
            HirType::Named(name) => self.named(ctx, name),
            HirType::Promise(inner) => {
                let settled = self.settled_type(ctx, inner)?;
                Ok(PromiseType::get(ctx, settled).into())
            }
            HirType::Unknown => Ok(perry_mir::JsonType::get(ctx).into()),
            HirType::Any => Err(LowerError::new(ShapeError::MissingTypeAnnotation).into()),
            HirType::Generic { base, type_args } => {
                if base == "WitInput" {
                    let [inner] = type_args.as_slice() else {
                        bail!("WitInput needs one type argument");
                    };
                    let inner = self.value_type(ctx, inner)?;
                    return self.input_type(ctx, inner);
                }
                if matches!(base.as_str(), "Map" | "ReadonlyMap") && !self.is_user_type(base) {
                    let [k, v] = type_args.as_slice() else {
                        bail!("{base} needs key and value type arguments");
                    };
                    return self.map_type(ctx, k, v);
                }
                if let Some(view) = CollectionViewKind::from_type_name(base)
                    && !self.is_user_type(base)
                {
                    return self.collection_view_source_type(ctx, view, type_args);
                }
                if base == "Record" && !self.is_user_type(base) {
                    let [k, v] = type_args.as_slice() else {
                        bail!("Record needs key and value type arguments");
                    };
                    return self.dict_source_type(ctx, k, v);
                }
                if matches!(base.as_str(), "Set" | "ReadonlySet") && !self.is_user_type(base) {
                    let [element] = type_args.as_slice() else {
                        bail!("{base} needs one element type argument");
                    };
                    return self.set_type(ctx, element);
                }
                if matches!(base.as_str(), "ReadonlyArray" | "Array") && !self.is_user_type(base) {
                    let [element] = type_args.as_slice() else {
                        bail!("{base} needs one element type argument");
                    };
                    let element = self.value_type(ctx, element)?;
                    return Ok(ArrayType::get(ctx, element).into());
                }
                if base == "ReadableStream"
                    && !self.is_user_type(base)
                    && matches!(
                        type_args.as_slice(),
                        [HirType::Named(n)] if n == "Uint8Array"
                    )
                {
                    return Ok(perry_mir::ReadableType::get(ctx).into());
                }
                self.generic(ctx, base, type_args)
            }
            HirType::Union(union_members) => self.union_type(ctx, union_members),
            HirType::Tuple(tuple_elements) => {
                let fields = tuple_elements
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| {
                        let ty = if let Some(int_ty) = self.integer_type(ctx, ty)
                            && int_ty.deref(ctx).width() == 64
                        {
                            int_ty.into()
                        } else {
                            self.value_type(ctx, ty)?
                        };
                        Ok((format!("_{index}"), ty))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(self.mir_tuple(ctx, fields)?.into())
            }
            HirType::Object(object) => {
                if let Some(val_ty) = &object.index_signature {
                    ensure!(
                        object.properties.is_empty(),
                        "index signatures are outside the closed-record profile when combined with fixed properties"
                    );
                    return self.dict_source_type(ctx, &HirType::String, val_ty);
                }
                Ok(self
                    .source_record(
                        ctx,
                        None,
                        object
                            .properties
                            .iter()
                            .map(|(name, p)| (name.as_str(), &p.ty, p.optional))
                            .collect(),
                    )?
                    .into())
            }
            HirType::Function(_) | HirType::Symbol | HirType::TypeVar(_) => {
                Err(LowerError::new(ShapeError::UnsupportedType {
                    ty: format!("{ty:?}"),
                })
                .into())
            }
        }
    }

    fn union_type(&self, ctx: &Context, members: &[HirType]) -> Result<TypeHandle> {
        let member_ids = TypeId::intern_slice(ctx, members);
        self.union_type_ids(ctx, &member_ids)
    }

    fn union_type_ids(&self, ctx: &Context, members: &[TypeId]) -> Result<TypeHandle> {
        let resolver = Resolver::new(ctx, self);
        let flattened = resolver.flatten(members, 0);
        let members = flattened.as_slice();
        if members
            .iter()
            .all(|ty| matches!(ty.hir(ctx), HirType::StringLiteral(_)))
        {
            return Ok(StringType::get(ctx).into());
        }
        let mut defined = Vec::with_capacity(members.len());
        let mut absence = None;
        let mut has_num = false;
        let mut has_str = false;
        let mut all_num_or_str = true;
        for &ty in members {
            match ty.hir(ctx) {
                HirType::Null => {
                    absence = crate::coerce::merge_absence(absence, Some(perry_mir::Absence::Null));
                }
                HirType::Void => {
                    absence =
                        crate::coerce::merge_absence(absence, Some(perry_mir::Absence::Undefined));
                }
                HirType::String => {
                    has_str = true;
                    defined.push(ty);
                }
                hir => {
                    if matches!(hir, HirType::Number)
                        || self
                            .integer_type(ctx, hir)
                            .is_some_and(|int_ty| int_ty.deref(ctx).width() <= 32)
                    {
                        has_num = true;
                    } else {
                        all_num_or_str = false;
                    }
                    defined.push(ty);
                }
            }
        }
        if let Some(absence) = absence {
            let inner = match defined.as_slice() {
                [defined] => self.value_type(ctx, defined.hir(ctx))?,
                [] => bail!("an absence-only union needs a declared value type"),
                defined => self.union_type_ids(ctx, defined)?,
            };
            return Ok(OptionalType::get_with(ctx, inner, absence).into());
        }
        if has_num && has_str && all_num_or_str {
            return Ok(perry_mir::NumberStringType::get(ctx).into());
        }
        let records = members
            .iter()
            .map(|&ty| {
                resolver
                    .record(ty, 0)
                    .context("unions other than tagged object types are not supported yet")
            })
            .collect::<Result<Vec<_>>>()?;
        if !records.is_empty()
            && records.iter().all(|props| {
                props.iter().any(|(name, prop_ty, _)| {
                    name == "ok" && matches!(prop_ty.hir(ctx), HirType::Boolean)
                })
            })
        {
            return self.result(ctx, &records);
        }
        self.variant(ctx, &records)
    }

    /// Build the record representation for an SDK `Result`, keeping disjoint payload slots that admit `undefined` when inactive.
    fn result(
        &self,
        ctx: &Context,
        alternatives: &[Vec<(String, TypeId, bool)>],
    ) -> Result<TypeHandle> {
        ensure!(
            alternatives.len() == 2
                || (alternatives.len() == 1
                    && alternatives[0].len() == 1
                    && alternatives[0][0].0 == "ok"),
            "a tagged result needs exactly two alternatives"
        );
        let mut value_arg = None;
        let mut error_arg = None;
        for properties in alternatives {
            for (p_name, prop_ty, _) in properties {
                match p_name.as_str() {
                    "ok" => {}
                    "value" => {
                        ensure!(
                            value_arg.replace(*prop_ty).is_none(),
                            "result payload value appears in both alternatives"
                        );
                    }
                    "error" => {
                        ensure!(
                            error_arg.replace(*prop_ty).is_none(),
                            "result payload error appears in both alternatives"
                        );
                    }
                    _ => bail!("result alternatives only contain ok, value and error"),
                }
            }
        }
        let mut fields = vec![(
            "ok".to_string(),
            IntegerType::get(ctx, 1, Signedness::Signless).into(),
        )];
        for (name, arg) in [("value", value_arg), ("error", error_arg)] {
            if let Some(prop_ty) = arg {
                let ty = self.value_type(ctx, prop_ty.hir(ctx))?;
                fields.push((name.to_string(), OptionalType::get(ctx, ty).into()));
            }
        }
        Ok(self.mir_record(ctx, fields)?.into())
    }

    fn variant(
        &self,
        ctx: &Context,
        members: &[Vec<(String, TypeId, bool)>],
    ) -> Result<TypeHandle> {
        let mut cases = Vec::with_capacity(members.len());
        for properties in members {
            let mut tag = None;
            let mut val_arg = None;
            let mut invalid = false;
            for (p_name, ty, _) in properties {
                match p_name.as_str() {
                    "tag" => {
                        let HirType::StringLiteral(lit) = ty.hir(ctx) else {
                            bail!("union members need a literal `tag`");
                        };
                        tag = Some(lit.clone());
                    }
                    "val" => val_arg = Some(*ty),
                    _ => invalid = true,
                }
            }
            let tag = tag.context("union members need a literal `tag`")?;
            ensure!(!invalid, "union members have `tag` and `val` only");
            ensure!(
                !cases.iter().any(|(name, _)| name == &tag),
                "variant tags must be distinct"
            );
            let payload = val_arg
                .map(|prop_ty| self.value_type(ctx, prop_ty.hir(ctx)))
                .transpose()?;
            cases.push((tag, payload));
        }
        Ok(perry_mir::VariantType::get(ctx, cases).into())
    }

    /// Return the MIR value type that a promise of `ty` settles with, mapping `void` to `undefined`.
    pub(crate) fn settled_type(&self, ctx: &Context, ty: &HirType) -> Result<TypeHandle> {
        match ty {
            HirType::Void => {
                let number = FP64Type::get(ctx).into();
                Ok(OptionalType::get(ctx, number).into())
            }
            ty => self.value_type(ctx, ty),
        }
    }

    pub(crate) fn integer_type(
        &self,
        ctx: &Context,
        ty: &HirType,
    ) -> Option<TypedHandle<IntegerType>> {
        let HirType::Named(name) = ty else {
            return None;
        };
        let (width, signedness) = self.decls.integer_spec(name)?;
        Some(IntegerType::get(ctx, width, signedness))
    }

    pub(super) fn foreign_value_type(
        &self,
        ctx: &Context,
        entry: &crate::hir::Foreign,
    ) -> Result<TypeHandle> {
        match entry {
            crate::hir::Foreign::Type(ty) => self.value_type(ctx, ty),
            crate::hir::Foreign::Integer { width, .. } => Ok(integer_mir_carrier(ctx, *width)),
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
