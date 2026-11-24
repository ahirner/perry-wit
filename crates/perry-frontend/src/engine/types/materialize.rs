//! Materialize dense solver types back into Perry HIR types.
use super::*;

pub(super) struct Materializer<'a, F> {
    pub(super) table: &'a TypeTable,
    pub(super) ctx: &'a Context,
    pub(super) locations: &'a [Ty],
    pub(super) dict_reads: &'a BitSet,
    pub(super) capture_free: &'a BitSet,
    pub(super) escaping_env: &'a BitSet,
    pub(super) class_name: &'a F,
    pub(super) visiting: Vec<LocId>,
}

impl<'a, F: Fn(ClassInstId) -> &'a str> Materializer<'_, F> {
    fn empty_object(index_signature: Option<Box<HirType>>) -> HirType {
        HirType::Object(ObjectType {
            name: None,
            properties: HashMap::new(),
            property_order: None,
            index_signature,
        })
    }
    fn run_cell(&mut self, loc: LocId, ty: Ty) -> HirType {
        self.visiting.push(loc);
        let hir = self.run(ty);
        self.visiting.pop();
        hir
    }
    fn run_slot(&mut self, ty: Ty) -> HirType {
        if self.table.is_zero_sized_func(ty, self.capture_free) {
            HirType::Number
        } else {
            self.run(ty)
        }
    }
    fn func_env_hir(ident: FuncIdentity) -> HirType {
        let mut properties = HashMap::with_capacity(2);
        properties.insert(
            "tag".to_owned(),
            PropertyInfo {
                ty: HirType::StringLiteral(func_tag(ident)),
                optional: false,
                readonly: false,
            },
        );
        properties.insert(
            "val".to_owned(),
            PropertyInfo {
                ty: HirType::Named(env_class_name(ident)),
                optional: false,
                readonly: false,
            },
        );
        HirType::Object(ObjectType {
            name: None,
            properties,
            property_order: Some(vec!["tag".to_owned(), "val".to_owned()]),
            index_signature: None,
        })
    }
    pub(super) fn run(&mut self, ty: Ty) -> HirType {
        match self.table.kind(ty) {
            TyKind::Bottom | TyKind::Open => HirType::Any,
            TyKind::Dynamic => HirType::Unknown,
            TyKind::Never => HirType::Never,
            TyKind::Void => HirType::Void,
            TyKind::Null => HirType::Null,
            TyKind::Boolean => HirType::Boolean,
            TyKind::Number => HirType::Number,
            TyKind::Int32 => HirType::Int32,
            TyKind::BigInt => HirType::BigInt,
            TyKind::String => HirType::String,
            TyKind::Symbol => HirType::Symbol,
            TyKind::Leaf(id) | TyKind::TypeVar(id) => id.hir(self.ctx).clone(),
            TyKind::Array(i) => HirType::Array(Box::new(self.run_slot(i))),
            TyKind::Promise(i) => {
                let inner = if i == Ty::BOTTOM {
                    HirType::Void
                } else {
                    self.run(i)
                };
                HirType::Promise(Box::new(inner))
            }
            TyKind::Cell { loc, kind } => {
                if self.visiting.contains(&loc) {
                    return match kind {
                        CellKind::Array | CellKind::LocalArray => {
                            HirType::Array(Box::new(HirType::Any))
                        }
                        CellKind::Promise => HirType::Promise(Box::new(HirType::Any)),
                        CellKind::Dict => Self::empty_object(None),
                    };
                }
                let elem = self.locations.get(loc.index()).copied().unwrap_or(Ty::OPEN);
                match kind {
                    CellKind::Array | CellKind::LocalArray => {
                        let inner = if self.table.is_zero_sized_func(elem, self.capture_free) {
                            HirType::Number
                        } else {
                            self.run_cell(loc, elem)
                        };
                        HirType::Array(Box::new(inner))
                    }
                    CellKind::Promise => {
                        let inner = if elem == Ty::BOTTOM {
                            HirType::Void
                        } else {
                            self.run_cell(loc, elem)
                        };
                        HirType::Promise(Box::new(inner))
                    }
                    CellKind::Dict => {
                        let idx = if self
                            .table
                            .is_valid_dict_value(elem, self.dict_reads.contains(loc.index()))
                        {
                            Some(Box::new(self.run_cell(loc, elem)))
                        } else {
                            None
                        };
                        Self::empty_object(idx)
                    }
                }
            }
            TyKind::Tuple(s) => HirType::Tuple(
                self.table
                    .ty_slice(s)
                    .iter()
                    .map(|&t| self.run_slot(t))
                    .collect(),
            ),
            TyKind::Union(s) => {
                let slice = self.table.ty_slice(s);
                let has_func = slice
                    .iter()
                    .any(|&m| matches!(self.table.kind(m), TyKind::Func(_)));
                let mut members = Vec::with_capacity(s.len());
                for &t in slice {
                    let h = if has_func && let TyKind::Func(ident) = self.table.kind(t) {
                        if self.escaping_env.contains(ident.template.index()) {
                            Self::func_env_hir(ident)
                        } else {
                            func_tag_hir(ident)
                        }
                    } else {
                        self.run(t)
                    };
                    if !members.contains(&h) {
                        members.push(h);
                    }
                }
                match members.len() {
                    0 => HirType::Never,
                    1 => members.pop().unwrap_or(HirType::Never),
                    _ => HirType::Union(members),
                }
            }
            TyKind::Generic { base, args } => HirType::Generic {
                base: match base.hir(self.ctx) {
                    HirType::Named(n) => n.clone(),
                    _ => String::new(),
                },
                type_args: self
                    .table
                    .ty_slice(args)
                    .iter()
                    .map(|&t| self.run(t))
                    .collect(),
            },
            TyKind::Object { props, index } => {
                let mut properties = HashMap::with_capacity(props.len());
                let mut order = Vec::with_capacity(props.len());
                for prop in self.table.object_slice(props) {
                    if self.table.is_zero_sized_func(prop.ty, self.capture_free) {
                        continue;
                    }
                    let name = self.table.name_str(prop.name).to_owned();
                    order.push(name.clone());
                    properties.insert(
                        name,
                        PropertyInfo {
                            ty: self.run(prop.ty),
                            optional: prop.optional,
                            readonly: prop.readonly,
                        },
                    );
                }
                let idx = index.map(|t| Box::new(self.run(t)));
                HirType::Object(ObjectType {
                    name: None,
                    properties,
                    property_order: (!order.is_empty()).then_some(order),
                    index_signature: idx,
                })
            }
            TyKind::ClassInst(inst) => HirType::Named((self.class_name)(inst).to_owned()),
            TyKind::Func(ident) if self.escaping_env.contains(ident.template.index()) => {
                Self::func_env_hir(ident)
            }
            TyKind::Func(_) => HirType::Function(perry_hir::types::FunctionType {
                params: Vec::new(),
                return_type: Box::new(HirType::Void),
                is_async: false,
                is_generator: false,
            }),
        }
    }
}

pub(crate) fn func_tag(ident: FuncIdentity) -> String {
    match ident.capture_owner {
        CaptureOwner::None => format!("fn_{}", ident.template.index()),
        CaptureOwner::Instance(owner) => format!("fn_{}_{}", ident.template.index(), owner.index()),
        CaptureOwner::Template(owner) => {
            format!("fn_{}_t{}", ident.template.index(), owner.index())
        }
    }
}

pub(crate) fn env_class_name(ident: FuncIdentity) -> String {
    format!("__Env_{}", func_tag(ident))
}

pub(crate) fn box_hir(inner: HirType) -> HirType {
    let mut properties = HashMap::with_capacity(1);
    properties.insert(
        "v".to_owned(),
        PropertyInfo {
            ty: inner,
            optional: false,
            readonly: false,
        },
    );
    HirType::Object(ObjectType {
        name: None,
        properties,
        property_order: Some(vec!["v".to_owned()]),
        index_signature: None,
    })
}

fn func_tag_hir(ident: FuncIdentity) -> HirType {
    let mut properties = HashMap::with_capacity(1);
    properties.insert(
        "tag".to_owned(),
        PropertyInfo {
            ty: HirType::StringLiteral(func_tag(ident)),
            optional: false,
            readonly: false,
        },
    );
    HirType::Object(ObjectType {
        name: None,
        properties,
        property_order: Some(vec!["tag".to_owned()]),
        index_signature: None,
    })
}
