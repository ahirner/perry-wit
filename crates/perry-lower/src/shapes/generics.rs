//! Instantiation of generic interfaces and type aliases by substitution.
use super::*;
use perry_hir::ir::{Interface, TypeAlias};

impl Shapes<'_> {
    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(super) fn generic(
        &self,
        ctx: &mut Context,
        base: &str,
        arguments: &[HirType],
    ) -> Result<TypeHandle> {
        let declaration = match self.hir.interfaces.iter().find(|i| i.name == base) {
            Some(interface) => Declaration::Interface(interface),
            None => Declaration::Alias(
                self.hir
                    .type_aliases
                    .iter()
                    .find(|a| a.name == base)
                    .with_context(|| {
                        format!("MIR backend does not support unresolved generic type {base}")
                    })?,
            ),
        };
        let parameters = match declaration {
            Declaration::Interface(interface) => &interface.type_params,
            Declaration::Alias(alias) => &alias.type_params,
        };
        ensure!(
            arguments.len() <= parameters.len(),
            "too many type arguments for {base}"
        );
        let mut substitutions = HashMap::new();
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = match arguments.get(index) {
                Some(argument) => argument.clone(),
                None => substitute(
                    parameter.default.as_deref().with_context(|| {
                        format!("missing type argument {} of {base}", parameter.name)
                    })?,
                    &substitutions,
                ),
            };
            substitutions.insert(parameter.name.as_str(), argument);
        }
        let actual: Vec<&HirType> = parameters
            .iter()
            .map(|parameter| &substitutions[parameter.name.as_str()])
            .collect();
        // Bound the size of the arguments, which a recursive default could grow without end.
        let mut work = actual.clone();
        let mut nodes = 0;
        while let Some(ty) = work.pop() {
            nodes += 1;
            ensure!(
                nodes < 4096,
                "generic type arguments expand indefinitely: {base}"
            );
            match ty {
                HirType::Array(inner) | HirType::Promise(inner) => work.push(inner),
                HirType::Tuple(items) | HirType::Union(items) => work.extend(items),
                HirType::Generic { type_args, .. } => work.extend(type_args),
                HirType::Object(object) => work.extend(object.properties.values().map(|p| &p.ty)),
                _ => {}
            }
        }
        let key = format!("{base}<{actual:?}>");
        if let Some(ty) = self.named.borrow().get(&key) {
            return Ok(*ty);
        }
        ensure!(
            self.pending.borrow().len() < 128,
            "generic type instantiation expands indefinitely: {base}"
        );
        let ty = match declaration {
            Declaration::Interface(interface) => {
                ensure!(
                    interface.extends.is_empty() && interface.methods.is_empty(),
                    "interface {base}: inheritance and methods are not supported yet"
                );
                let properties: Vec<_> = interface
                    .properties
                    .iter()
                    .map(|p| {
                        (
                            p.name.as_str(),
                            substitute(&p.ty, &substitutions),
                            p.optional,
                        )
                    })
                    .collect();
                self.hir_record(
                    ctx,
                    Some(&key),
                    properties
                        .iter()
                        .map(|(name, ty, optional)| (*name, ty, *optional))
                        .collect(),
                )?
                .into()
            }
            Declaration::Alias(alias) => {
                let value = substitute(&alias.ty, &substitutions);
                if let HirType::Object(object) = &value {
                    self.hir_record(
                        ctx,
                        Some(&key),
                        object
                            .properties
                            .iter()
                            .map(|(name, p)| (name.as_str(), &p.ty, p.optional))
                            .collect(),
                    )?
                    .into()
                } else {
                    ensure!(
                        !self.pending.borrow().contains_key(&key),
                        "nonproductive recursive type alias {base}"
                    );
                    self.pending.borrow_mut().insert(key.clone(), None);
                    let value = self.value_type(ctx, &value);
                    self.pending.borrow_mut().remove(&key);
                    value?
                }
            }
        };
        if !self.pending.borrow().contains_key(&key) {
            self.named.borrow_mut().insert(key, ty);
        }
        Ok(ty)
    }
}

/// The generic declaration an instantiation expands.
#[derive(Clone, Copy)]
enum Declaration<'a> {
    Interface(&'a Interface),
    Alias(&'a TypeAlias),
}

/// `ty` with its type variables (and same-named type references) replaced by `arguments`.
fn substitute(ty: &HirType, arguments: &HashMap<&str, HirType>) -> HirType {
    match ty {
        HirType::TypeVar(name) | HirType::Named(name) if arguments.contains_key(name.as_str()) => {
            arguments[name.as_str()].clone()
        }
        HirType::Array(element) => HirType::Array(Box::new(substitute(element, arguments))),
        HirType::Promise(element) => HirType::Promise(Box::new(substitute(element, arguments))),
        HirType::Tuple(elements) => {
            HirType::Tuple(elements.iter().map(|t| substitute(t, arguments)).collect())
        }
        HirType::Union(elements) => {
            HirType::Union(elements.iter().map(|t| substitute(t, arguments)).collect())
        }
        HirType::Generic { base, type_args } => HirType::Generic {
            base: base.clone(),
            type_args: type_args.iter().map(|t| substitute(t, arguments)).collect(),
        },
        HirType::Object(object) => {
            let mut object = object.clone();
            for property in object.properties.values_mut() {
                property.ty = substitute(&property.ty, arguments);
            }
            HirType::Object(object)
        }
        _ => ty.clone(),
    }
}
