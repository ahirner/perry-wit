use super::*;

pub(super) struct FunctionEnv<'a> {
    pub(super) generic_functions: Option<&'a HashMap<FuncId, perry_hir::ir::Function>>,
    pub(super) instantiated_returns: HashMap<FuncId, HirType>,
    pub(super) ctx: &'a Context,
    pub(super) base: &'a perry_hir::HirTypeEnv,
    pub(super) interfaces: &'a [perry_hir::ir::Interface],
    pub(super) type_aliases: &'a [perry_hir::ir::TypeAlias],
    pub(super) classes: &'a [perry_hir::ir::Class],
    pub(super) locals: &'a mut HashMap<LocalId, HirType>,
    pub(super) integer_aliases: &'a HashMap<String, HirType>,
    pub(super) imported_types: &'a HashMap<String, String>,
    pub(super) foreign: &'a HashMap<String, crate::hir::Foreign>,
    pub(super) metadata: &'a hir_compat::Metadata<'a>,
}

const NUMBER_TYPE: HirType = HirType::Number;
const BIGINT_TYPE: HirType = HirType::BigInt;

impl<'a> FunctionEnv<'a> {
    pub(super) fn normalize<'t>(&self, ty: &'t HirType) -> &'t HirType {
        match ty {
            HirType::Named(name) => match self.integer_aliases.get(name) {
                Some(HirType::BigInt) => &BIGINT_TYPE,
                Some(_) => &NUMBER_TYPE,
                None => ty,
            },
            ty => ty,
        }
    }

    pub(super) fn normalize_owned(&self, mut ty: HirType) -> HirType {
        ty.transform(&mut |node| {
            if let HirType::Named(name) = node
                && let Some(alias) = self.integer_aliases.get(name.as_str())
            {
                *node = alias.clone();
            }
        });
        ty
    }

    pub(super) fn resolve_foreign_type(&self, type_name: &str) -> Option<&'a HirType> {
        let mut current = self
            .imported_types
            .get(type_name)
            .map_or(type_name, String::as_str);
        for _ in 0..8 {
            match self.foreign.get(current)? {
                crate::hir::Foreign::Type(HirType::Named(next)) if next != current => {
                    current = next;
                }
                crate::hir::Foreign::Type(ty) => return Some(ty),
                _ => return None,
            }
        }
        None
    }

    pub(super) fn class_member_type(
        &self,
        base: &str,
        type_args: &[HirType],
        property: &str,
    ) -> Option<HirType> {
        let class = self.classes.iter().find(|c| c.name == base)?;
        let mut ty = self.named_property_type(base, property)?;
        if !class.type_params.is_empty() && !type_args.is_empty() {
            let bounds: HashMap<&str, &HirType> = class
                .type_params
                .iter()
                .zip(type_args.iter())
                .map(|(tp, arg)| (tp.name.as_str(), arg))
                .collect();
            substitute_type_vars(&mut ty, &bounds);
        }
        Some(self.normalize_owned(ty))
    }

    pub(super) fn class_method_return_type(
        &mut self,
        object: &Expr,
        name: &str,
    ) -> Option<HirType> {
        if self.classes.is_empty() {
            return None;
        }
        if let Expr::ClassRef(class_name) = object {
            return self
                .static_method_return_type(class_name, name)
                .cloned()
                .or_else(|| {
                    self.classes
                        .iter()
                        .find(|c| c.name == *class_name)?
                        .static_methods
                        .iter()
                        .find(|m| m.name == name)
                        .map(|m| self.normalize_owned(m.return_type.clone()))
                });
        }
        match strip_nullish(expression(object, self)).0 {
            HirType::Named(base) => match self.class_member_type(&base, &[], name)? {
                HirType::Function(ft) => Some(*ft.return_type),
                _ => None,
            },
            HirType::Generic { base, type_args } => {
                match self.class_member_type(&base, &type_args, name)? {
                    HirType::Function(ft) => Some(*ft.return_type),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

impl HirTypeFacts for FunctionEnv<'_> {
    fn local_type(&self, id: LocalId) -> Option<&HirType> {
        self.locals
            .get(&id)
            .or_else(|| self.base.local_type(id))
            .map(|ty| self.normalize(ty))
    }

    fn global_type(&self, id: perry_hir::types::GlobalId) -> Option<&HirType> {
        self.base.global_type(id).map(|ty| self.normalize(ty))
    }

    fn function_return_type(&self, id: FuncId) -> Option<&HirType> {
        self.instantiated_returns
            .get(&id)
            .or_else(|| self.base.function_return_type(id))
            .map(|ty| self.normalize(ty))
    }

    fn extern_function_return_type(&self, name: &str) -> Option<&HirType> {
        self.base
            .extern_function_return_type(name)
            .map(|ty| self.normalize(ty))
    }

    fn this_type(&self) -> Option<HirType> {
        self.base.this_type()
    }

    fn enum_member_type(&self, enum_name: &str, member_name: &str) -> Option<HirType> {
        self.base.enum_member_type(enum_name, member_name)
    }

    fn static_field_type(&self, class_name: &str, field_name: &str) -> Option<&HirType> {
        self.base
            .static_field_type(class_name, field_name)
            .map(|ty| self.normalize(ty))
    }

    fn static_method_return_type(&self, class_name: &str, method_name: &str) -> Option<&HirType> {
        self.base
            .static_method_return_type(class_name, method_name)
            .map(|ty| self.normalize(ty))
    }

    fn named_property_type(&self, type_name: &str, property: &str) -> Option<HirType> {
        match (type_name, property) {
            ("Temporal.PlainDate", "year" | "month" | "day" | "dayOfWeek")
            | (
                "Temporal.PlainDateTime",
                "year" | "month" | "day" | "dayOfWeek" | "hour" | "minute" | "second"
                | "millisecond" | "microsecond" | "nanosecond",
            )
            | ("Temporal.Instant", "epochMilliseconds") => return Some(HirType::Number),
            _ => {}
        }
        let ty = self
            .base
            .named_property_type(type_name, property)
            .or_else(|| {
                let HirType::Object(object) = self.resolve_foreign_type(type_name)? else {
                    return None;
                };
                let prop = object.properties.get(property)?;
                Some(shapes::generics::optional_property(
                    prop.ty.clone(),
                    prop.optional,
                ))
            })?;
        Some(self.normalize_owned(ty))
    }

    fn super_property_type(&self, property: &str) -> Option<HirType> {
        self.base.super_property_type(property)
    }

    fn super_method_return_type(&self, method: &str) -> Option<HirType> {
        self.base.super_method_return_type(method)
    }
}
