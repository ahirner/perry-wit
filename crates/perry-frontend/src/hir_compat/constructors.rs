//! Compilation-local constructor types, indexed independently of HIR traversal order.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{bail_diag, bail_internal, codes, internal, unsupported};
use swc_ecma_visit::{VisitMut, VisitMutWith};

const CONSTRUCTOR: &str = "\0perry_constructor:";

const MAP: &str = "\0perry_Map";
const SET: &str = "\0perry_Set";

pub(crate) fn typed_collection(is_map: bool, type_args: Vec<HirType>, args: Vec<Expr>) -> Expr {
    Expr::New {
        class_name: if is_map { MAP } else { SET }.into(),
        type_args,
        args,
        byte_offset: 0,
        byte_end: 0,
        cap_args_appended: 0,
    }
}

#[derive(Default)]
pub(super) struct Constructors(Vec<ConstructorTypes>);

enum ConstructorTypes {
    Map(Option<(HirType, HirType)>),
    Set(Option<HirType>),
}

pub enum Constructor<'e> {
    Map {
        types: Option<(&'e HirType, &'e HirType)>,
        init: Option<&'e Expr>,
    },
    Set {
        element: Option<&'e HirType>,
        init: Option<&'e Expr>,
    },
}

pub fn constructor(expr: &Expr) -> Option<Constructor<'_>> {
    let Expr::New {
        class_name,
        type_args,
        args,
        ..
    } = expr
    else {
        return None;
    };
    match class_name.as_str() {
        MAP => Some(Constructor::Map {
            types: match type_args.as_slice() {
                [key, value] => Some((key, value)),
                _ => None,
            },
            init: args.first(),
        }),
        SET => Some(Constructor::Set {
            element: type_args.first(),
            init: args.first(),
        }),
        _ => None,
    }
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
                .ok_or_else(|| internal!("no constructor was preserved as {:?}", alias.name))?;
            let HirType::Generic { type_args, .. } = alias.ty else {
                bail_internal!(
                    "the type arguments of a Map or Set constructor did not survive as generics"
                );
            };
            match entry {
                ConstructorTypes::Map(types) => {
                    let Ok([key, value]) = <[HirType; 2]>::try_from(type_args) else {
                        bail_diag!(
                            codes::TYPE_MISMATCH,
                            "new Map takes key and value type arguments"
                        );
                    };
                    *types = Some((key, value));
                }
                ConstructorTypes::Set(element) => {
                    let Ok([item]) = <[HirType; 1]>::try_from(type_args) else {
                        bail_diag!(
                            codes::TYPE_MISMATCH,
                            "new Set takes one element type argument"
                        );
                    };
                    *element = Some(item);
                }
            }
        }
        Ok(())
    }

    pub(super) fn rewrite_expr(&self, expr: &mut Expr) {
        let argument = match expr {
            Expr::NewDynamic { callee, args, .. }
                if matches!(&**callee, Expr::PropertyGet { object, property, .. }
                    if matches!(&**object, Expr::GlobalGet(0)) && property == "Map") =>
            {
                args.first_mut()
            }
            Expr::MapNewFromArray(argument) | Expr::SetNewFromArray(argument) => {
                Some(&mut **argument)
            }
            Expr::New {
                class_name, args, ..
            } if matches!(class_name.as_str(), "Map" | "Set") => args.first_mut(),
            _ => None,
        };
        let Some(Expr::Sequence(items)) = argument else {
            return;
        };
        let Some(Expr::String(token)) = items.first() else {
            return;
        };
        let Some(id) = token
            .strip_prefix(CONSTRUCTOR)
            .and_then(|s| s.parse::<usize>().ok())
        else {
            return;
        };
        let Some(entry) = self.0.get(id) else {
            return;
        };
        let init = (items.len() > 1)
            .then(|| items.remove(1))
            .filter(|e| !matches!(e, Expr::Undefined | Expr::Null));
        let args = init.into_iter().collect();
        *expr = match entry {
            ConstructorTypes::Map(types) => typed_collection(
                true,
                types
                    .as_ref()
                    .map(|(k, v)| vec![k.clone(), v.clone()])
                    .unwrap_or_default(),
                args,
            ),
            ConstructorTypes::Set(element) => typed_collection(
                false,
                element
                    .as_ref()
                    .map(|e| vec![e.clone()])
                    .unwrap_or_default(),
                args,
            ),
        };
    }
}

pub(super) fn preserve(
    module: &mut ast::Module,
    unresolved: SyntaxContext,
) -> Result<Constructors> {
    struct Preserver {
        registry: Constructors,
        unresolved: SyntaxContext,
        error: Option<perry_wit_diagnostics::Diagnostic>,
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
                    unsupported!("{} takes at most one iterable argument", ident.sym)
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
