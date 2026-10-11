use super::*;

impl Shapes<'_> {
    pub(super) fn generic(
        &self,
        ctx: &mut Context,
        base: &str,
        arguments: &[HirType],
    ) -> Result<TypeHandle> {
        let interface = self.hir.interfaces.iter().find(|i| i.name == base);
        let alias = self.hir.type_aliases.iter().find(|a| a.name == base);
        let parameters = interface
            .map(|i| &i.type_params)
            .or_else(|| alias.map(|a| &a.type_params))
            .with_context(|| {
                format!("MIR backend does not support unresolved generic type {base}")
            })?;
        ensure!(
            arguments.len() <= parameters.len(),
            "too many type arguments for {base}"
        );
        let mut substitutions = HashMap::new();
        let mut actual = Vec::with_capacity(parameters.len());
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
            substitutions.insert(parameter.name.clone(), argument.clone());
            actual.push(argument);
        }
        let mut work: Vec<_> = actual.iter().collect();
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
        let ty = if let Some(interface) = interface {
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
        } else {
            let value = substitute(&alias.expect("resolved declaration").ty, &substitutions);
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
        };
        if !self.pending.borrow().contains_key(&key) {
            self.named.borrow_mut().insert(key, ty);
        }
        Ok(ty)
    }
}

fn substitute(ty: &HirType, arguments: &HashMap<String, HirType>) -> HirType {
    match ty {
        HirType::TypeVar(name) | HirType::Named(name) if arguments.contains_key(name) => {
            arguments[name].clone()
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
