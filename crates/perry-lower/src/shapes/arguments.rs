//! Source argument identities, independent of MIR carriers and diagnostic formatting.
use anyhow::{Result, bail, ensure};
use perry_hir::types::{FunctionType, ObjectType, PropertyInfo, Type as HirType};
use pliron::{
    context::Context,
    derive::{format, pliron_type},
    r#type::{Type, TypedHandle},
};
use std::collections::HashMap;

pub(super) type ArgumentId = TypedHandle<Argument>;

#[pliron_type(name = "perry.source_argument", format, verifier = "succ")]
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Argument {
    kind: Kind,
}

#[format]
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    Void,
    Null,
    Boolean,
    Number,
    Int32,
    BigInt,
    String,
    StringLiteral(String),
    Symbol,
    Array(ArgumentId),
    #[format("`[` vec($0, Char(`,`)) `]`")]
    Tuple(Vec<ArgumentId>),
    #[format(
        "`{` opt($name) `;` vec($properties, Char(`,`)) `;` opt($property_order) `;` opt($index_signature) `}`"
    )]
    Object {
        name: Option<String>,
        properties: Vec<Property>,
        property_order: Option<PropertyOrder>,
        index_signature: Option<ArgumentId>,
    },
    #[format("`(` vec($params, Char(`,`)) `;` $return_type `;` $is_async `;` $is_generator `)`")]
    Function {
        params: Vec<Parameter>,
        return_type: ArgumentId,
        is_async: bool,
        is_generator: bool,
    },
    #[format("`[` vec($0, Char(`,`)) `]`")]
    Union(Vec<ArgumentId>),
    Promise(ArgumentId),
    Any,
    Unknown,
    Never,
    Named(String),
    #[format("`<` $base `;` vec($arguments, Char(`,`)) `>`")]
    Generic {
        base: String,
        arguments: Vec<ArgumentId>,
    },
}

#[format("`[` vec($0, Char(`,`)) `]`")]
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct PropertyOrder(Vec<String>);

#[format]
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Property {
    pub(super) name: String,
    pub(super) ty: ArgumentId,
    pub(super) optional: bool,
    readonly: bool,
}

#[format]
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Parameter {
    name: String,
    ty: ArgumentId,
    optional: bool,
}

impl Argument {
    pub(super) fn from_kind(ctx: &Context, kind: Kind) -> ArgumentId {
        Type::instantiate(Self { kind }, ctx)
    }

    pub(super) fn from_hir(
        ctx: &Context,
        ty: &HirType,
        bindings: &HashMap<&str, ArgumentId>,
        remaining: &mut usize,
    ) -> Result<ArgumentId> {
        ensure!(*remaining > 1, "generic type arguments expand indefinitely");
        *remaining -= 1;
        if let HirType::Named(name) | HirType::TypeVar(name) = ty
            && let Some(argument) = bindings.get(name.as_str())
        {
            return Ok(*argument);
        }
        let kind = match ty {
            HirType::Void => Kind::Void,
            HirType::Null => Kind::Null,
            HirType::Boolean => Kind::Boolean,
            HirType::Number => Kind::Number,
            HirType::Int32 => Kind::Int32,
            HirType::BigInt => Kind::BigInt,
            HirType::String => Kind::String,
            HirType::StringLiteral(value) => Kind::StringLiteral(value.clone()),
            HirType::Symbol => Kind::Symbol,
            HirType::Array(element) => {
                Kind::Array(Self::from_hir(ctx, element, bindings, remaining)?)
            }
            HirType::Tuple(elements) => Kind::Tuple(
                elements
                    .iter()
                    .map(|ty| Self::from_hir(ctx, ty, bindings, remaining))
                    .collect::<Result<_>>()?,
            ),
            HirType::Object(object) => {
                let mut properties = object
                    .properties
                    .iter()
                    .map(|(name, property)| {
                        Ok(Property {
                            name: name.clone(),
                            ty: Self::from_hir(ctx, &property.ty, bindings, remaining)?,
                            optional: property.optional,
                            readonly: property.readonly,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                properties.sort_unstable_by(|a, b| a.name.cmp(&b.name));
                Kind::Object {
                    name: object.name.clone(),
                    properties,
                    property_order: object.property_order.clone().map(PropertyOrder),
                    index_signature: object
                        .index_signature
                        .as_deref()
                        .map(|ty| Self::from_hir(ctx, ty, bindings, remaining))
                        .transpose()?,
                }
            }
            HirType::Function(function) => Kind::Function {
                params: function
                    .params
                    .iter()
                    .map(|(name, ty, optional)| {
                        Ok(Parameter {
                            name: name.clone(),
                            ty: Self::from_hir(ctx, ty, bindings, remaining)?,
                            optional: *optional,
                        })
                    })
                    .collect::<Result<_>>()?,
                return_type: Self::from_hir(ctx, &function.return_type, bindings, remaining)?,
                is_async: function.is_async,
                is_generator: function.is_generator,
            },
            HirType::Union(members) => Kind::Union(
                members
                    .iter()
                    .map(|ty| Self::from_hir(ctx, ty, bindings, remaining))
                    .collect::<Result<_>>()?,
            ),
            HirType::Promise(inner) => {
                Kind::Promise(Self::from_hir(ctx, inner, bindings, remaining)?)
            }
            HirType::Any => Kind::Any,
            HirType::Unknown => Kind::Unknown,
            HirType::Never => Kind::Never,
            HirType::Named(name) => Kind::Named(name.clone()),
            HirType::Generic { base, type_args } => {
                let available = (*remaining).min(4096);
                let mut argument_budget = available;
                let arguments = type_args
                    .iter()
                    .map(|ty| Self::from_hir(ctx, ty, bindings, &mut argument_budget))
                    .collect::<Result<_>>()?;
                *remaining -= available - argument_budget;
                Kind::Generic {
                    base: base.clone(),
                    arguments,
                }
            }
            HirType::TypeVar(name) => bail!("unresolved type argument {name}"),
        };
        Ok(Type::instantiate(Self { kind }, ctx))
    }

    pub(super) fn kind(&self) -> &Kind {
        &self.kind
    }

    pub(super) fn property(&self, name: &str) -> Option<&Property> {
        let Kind::Object { properties, .. } = &self.kind else {
            return None;
        };
        properties
            .binary_search_by(|property| property.name.as_str().cmp(name))
            .ok()
            .map(|index| &properties[index])
    }

    pub(super) fn to_hir(&self, ctx: &Context) -> HirType {
        let child = |id: &ArgumentId| id.deref(ctx).to_hir(ctx);
        match &self.kind {
            Kind::Void => HirType::Void,
            Kind::Null => HirType::Null,
            Kind::Boolean => HirType::Boolean,
            Kind::Number => HirType::Number,
            Kind::Int32 => HirType::Int32,
            Kind::BigInt => HirType::BigInt,
            Kind::String => HirType::String,
            Kind::StringLiteral(value) => HirType::StringLiteral(value.clone()),
            Kind::Symbol => HirType::Symbol,
            Kind::Array(element) => HirType::Array(Box::new(child(element))),
            Kind::Tuple(elements) => HirType::Tuple(elements.iter().map(child).collect()),
            Kind::Union(members) => HirType::Union(members.iter().map(child).collect()),
            Kind::Promise(inner) => HirType::Promise(Box::new(child(inner))),
            Kind::Any => HirType::Any,
            Kind::Unknown => HirType::Unknown,
            Kind::Never => HirType::Never,
            Kind::Named(name) => HirType::Named(name.clone()),
            Kind::Generic { base, arguments } => HirType::Generic {
                base: base.clone(),
                type_args: arguments.iter().map(child).collect(),
            },
            Kind::Object {
                name,
                properties,
                property_order,
                index_signature,
            } => HirType::Object(ObjectType {
                name: name.clone(),
                properties: properties
                    .iter()
                    .map(|property| {
                        (
                            property.name.clone(),
                            PropertyInfo {
                                ty: child(&property.ty),
                                optional: property.optional,
                                readonly: property.readonly,
                            },
                        )
                    })
                    .collect(),
                property_order: property_order.as_ref().map(|order| order.0.clone()),
                index_signature: index_signature.map(|id| Box::new(child(&id))),
            }),
            Kind::Function {
                params,
                return_type,
                is_async,
                is_generator,
            } => HirType::Function(FunctionType {
                params: params
                    .iter()
                    .map(|param| (param.name.clone(), child(&param.ty), param.optional))
                    .collect(),
                return_type: Box::new(child(return_type)),
                is_async: *is_async,
                is_generator: *is_generator,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perry_hir::types::{ObjectType, PropertyInfo};
    use pliron::{irfmt::parsers::type_parser, parsable::parse_from_str, printable::Printable};

    fn intern(ctx: &Context, ty: &HirType) -> Result<ArgumentId> {
        Argument::from_hir(ctx, ty, &HashMap::new(), &mut 4096)
    }

    #[test]
    fn argument_identity_preserves_source_contracts() {
        let ctx = &mut Context::new();
        let property = |ty| PropertyInfo {
            ty,
            optional: false,
            readonly: false,
        };
        let object = ObjectType {
            name: None,
            properties: [
                (
                    "second".into(),
                    property(HirType::Array(Box::new(HirType::Number))),
                ),
                (
                    "first".into(),
                    property(HirType::StringLiteral("quoted value".into())),
                ),
            ]
            .into_iter()
            .collect(),
            property_order: Some(vec!["first".into(), "second".into()]),
            index_signature: None,
        };
        let expected = intern(ctx, &HirType::Object(object.clone())).unwrap();
        assert_eq!(
            expected.deref(ctx).to_hir(ctx),
            HirType::Object(object.clone())
        );
        let mut reordered = object.clone();
        let mut properties: Vec<_> = reordered.properties.into_iter().collect();
        properties.sort_by(|a, b| a.0.cmp(&b.0));
        reordered.properties = properties.into_iter().rev().collect();
        assert_eq!(expected, intern(ctx, &HirType::Object(reordered)).unwrap());
        let printed = expected.disp(ctx).to_string();
        assert_eq!(
            expected.to_handle(),
            parse_from_str(type_parser(), ctx, &printed).unwrap()
        );

        let changes: [fn(&mut ObjectType); 5] = [
            |ty| ty.properties.get_mut("first").unwrap().optional = true,
            |ty| ty.properties.get_mut("first").unwrap().readonly = true,
            |ty| ty.property_order.as_mut().unwrap().reverse(),
            |ty| ty.properties.get_mut("first").unwrap().ty = HirType::String,
            |ty| ty.index_signature = Some(Box::new(HirType::Number)),
        ];
        for change in changes {
            let mut changed = object.clone();
            change(&mut changed);
            assert_ne!(expected, intern(ctx, &HirType::Object(changed)).unwrap());
        }
        assert_ne!(
            intern(ctx, &HirType::Number).unwrap(),
            intern(ctx, &HirType::Int32).unwrap()
        );
        assert_ne!(
            intern(ctx, &HirType::BigInt).unwrap(),
            intern(ctx, &HirType::Int32).unwrap()
        );
        assert!(intern(ctx, &HirType::Array(Box::new(HirType::TypeVar("T".into())))).is_err());
        let named = intern(ctx, &HirType::Named("T".into())).unwrap();
        let bindings = [("T", named)].into_iter().collect();
        let pair = Argument::from_hir(
            ctx,
            &HirType::Tuple(vec![
                HirType::TypeVar("T".into()),
                HirType::Named("T".into()),
            ]),
            &bindings,
            &mut 4096,
        )
        .unwrap();
        assert_eq!(
            pair.deref(ctx).to_hir(ctx),
            HirType::Tuple(vec![HirType::Named("T".into()), HirType::Named("T".into()),])
        );
    }
}
