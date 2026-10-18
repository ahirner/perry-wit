//! Compilation-local constructor types, indexed independently of HIR traversal order.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{VisitMut, VisitMutWith};

const CONSTRUCTOR: &str = "\0perry_constructor:";

#[derive(Default)]
pub(crate) struct Constructors(Vec<ConstructorTypes>);

enum ConstructorTypes {
    Map(Option<(HirType, HirType)>),
    Set(Option<HirType>),
}

pub(crate) enum Constructor<'a> {
    Map {
        types: Option<&'a (HirType, HirType)>,
        init: Option<&'a Expr>,
    },
    Set {
        element: Option<&'a HirType>,
        init: Option<&'a Expr>,
    },
}

impl Constructors {
    /// Move the type arguments of the aliases [preserve] declared into their constructors,
    /// removing those aliases from `module`.
    pub(super) fn collect_types(&mut self, module: &mut HirModule) -> Result<()> {
        for alias in std::mem::take(&mut module.type_aliases) {
            let Some(index) = alias
                .name
                .strip_prefix(CONSTRUCTOR)
                .and_then(|id| id.parse::<usize>().ok())
            else {
                module.type_aliases.push(alias);
                continue;
            };
            let entry = self
                .0
                .get_mut(index)
                .with_context(|| format!("no constructor was preserved as {:?}", alias.name))?;
            let HirType::Generic { type_args, .. } = alias.ty else {
                bail!("the type arguments of a Map or Set constructor did not survive as generics");
            };
            match entry {
                ConstructorTypes::Map(types) => {
                    let Ok([key, value]) = <[HirType; 2]>::try_from(type_args) else {
                        bail!("new Map takes key and value type arguments");
                    };
                    *types = Some((key, value));
                }
                ConstructorTypes::Set(element) => {
                    let Ok([item]) = <[HirType; 1]>::try_from(type_args) else {
                        bail!("new Set takes one element type argument");
                    };
                    *element = Some(item);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn get<'a>(&'a self, expr: &'a Expr) -> Option<Constructor<'a>> {
        let argument = match expr {
            Expr::NewDynamic { callee, args, .. }
                if matches!(&**callee, Expr::PropertyGet { object, property, .. }
                    if matches!(&**object, Expr::GlobalGet(0)) && property == "Map") =>
            {
                args.first()?
            }
            Expr::MapNewFromArray(argument) | Expr::SetNewFromArray(argument) => argument,
            Expr::New {
                class_name, args, ..
            } if matches!(class_name.as_str(), "Map" | "Set") => args.first()?,
            _ => return None,
        };
        let Expr::Sequence(items) = argument else {
            return None;
        };
        let (Expr::String(token), rest) = items.split_first()? else {
            return None;
        };
        let id: usize = token.strip_prefix(CONSTRUCTOR)?.parse().ok()?;
        let init = rest
            .first()
            .filter(|expr| !matches!(expr, Expr::Undefined | Expr::Null));
        Some(match self.0.get(id)? {
            ConstructorTypes::Map(types) => Constructor::Map {
                types: types.as_ref(),
                init,
            },
            ConstructorTypes::Set(element) => Constructor::Set {
                element: element.as_ref(),
                init,
            },
        })
    }
}

pub(super) fn preserve(
    module: &mut ast::Module,
    unresolved: SyntaxContext,
) -> Result<Constructors> {
    struct Preserver {
        registry: Constructors,
        unresolved: SyntaxContext,
        error: Option<anyhow::Error>,
        aliases: Vec<ast::ModuleItem>,
    }
    impl VisitMut for Preserver {
        fn visit_mut_new_expr(&mut self, new: &mut ast::NewExpr) {
            new.visit_mut_children_with(self);
            let ast::Expr::Ident(ident) = &*new.callee else {
                return;
            };
            if ident.ctxt != self.unresolved {
                return;
            }
            let entry = match ident.sym.as_ref() {
                "Map" => ConstructorTypes::Map(None),
                "Set" => ConstructorTypes::Set(None),
                _ => return,
            };
            let args = new.args.get_or_insert_default();
            if args.len() > 1 || args.iter().any(|arg| arg.spread.is_some()) {
                // Keep the first error; later ones may be its consequences.
                self.error.get_or_insert_with(|| {
                    anyhow!("{} takes at most one iterable argument", ident.sym)
                });
                return;
            }
            let token = format!("{CONSTRUCTOR}{}", self.registry.0.len());
            // Let Perry's type extractor preserve the full annotation syntax.
            if new.type_args.is_some() {
                self.aliases.push(ast::ModuleItem::Stmt(ast::Stmt::Decl(
                    ast::Decl::TsTypeAlias(Box::new(ast::TsTypeAliasDecl {
                        span: new.span,
                        declare: false,
                        id: ast::Ident::new(token.clone().into(), new.span, SyntaxContext::empty()),
                        type_params: None,
                        type_ann: Box::new(ast::TsType::TsTypeRef(ast::TsTypeRef {
                            span: new.span,
                            type_name: ast::TsEntityName::Ident(ident.clone()),
                            type_params: new.type_args.clone(),
                        })),
                    })),
                )));
            }
            self.registry.0.push(entry);
            let token = Box::new(ast::Expr::Lit(ast::Lit::Str(ast::Str {
                span: new.span,
                value: token.into(),
                raw: None,
            })));
            let init = args.pop().map(|arg| arg.expr).unwrap_or_else(|| {
                Box::new(ast::Expr::Lit(ast::Lit::Null(ast::Null { span: new.span })))
            });
            args.push(ast::ExprOrSpread {
                spread: None,
                expr: Box::new(ast::Expr::Seq(ast::SeqExpr {
                    span: new.span,
                    exprs: vec![token, init],
                })),
            });
        }
    }
    let mut preserver = Preserver {
        registry: Constructors::default(),
        unresolved,
        error: None,
        aliases: Vec::new(),
    };
    module.visit_mut_with(&mut preserver);
    if let Some(error) = preserver.error {
        return Err(error);
    }
    module.body.extend(preserver.aliases);
    Ok(preserver.registry)
}
