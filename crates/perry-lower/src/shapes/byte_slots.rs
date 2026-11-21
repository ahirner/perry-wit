//! Materialize storage required by shared MIR aggregate slots.
use super::*;
use crate::{FuncId, LocalId, optional_parts};
use pliron::value::Value;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Binding {
    Local(LocalId),
    Result(FuncId),
}

pub(crate) enum Storage {
    Source {
        views: RefCell<Vec<Value>>,
        bindings: RefCell<Vec<(Binding, Value)>>,
    },
    Materialized(HashMap<Binding, Vec<Vec<usize>>>),
}

impl Default for Storage {
    fn default() -> Self {
        Self::Source {
            views: Default::default(),
            bindings: Default::default(),
        }
    }
}

impl Storage {
    pub(crate) fn record(&self, id: LocalId, slot: Value) {
        if let Self::Source { bindings, .. } = self {
            bindings.borrow_mut().push((Binding::Local(id), slot));
        }
    }
}
impl Shapes<'_> {
    pub(crate) fn storage(
        &self,
        ctx: &Context,
        binding: Binding,
        ty: TypeHandle,
    ) -> Result<TypeHandle> {
        let Storage::Materialized(ref plans) = self.byte_slots else {
            return Ok(ty);
        };
        let Some(paths) = plans.get(&binding) else {
            return Ok(ty);
        };
        self.storage_type(
            ctx,
            ty,
            &paths.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        )
    }
    fn storage_type(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        paths: &[&[usize]],
    ) -> Result<TypeHandle> {
        if paths.is_empty() {
            return Ok(ty);
        }
        if let Some((inner, absence)) = optional_parts(ctx, ty) {
            return Ok(
                OptionalType::get_with(ctx, self.storage_type(ctx, inner, paths)?, absence).into(),
            );
        }
        if paths.iter().any(|p| p.is_empty()) {
            return Ok(perry_mir::byte_input(ctx));
        }
        let suffixes = |step: usize| {
            paths
                .iter()
                .filter_map(|path| match path.split_first() {
                    Some((head, tail)) if head == &step => Some(tail),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let value = ty.deref(ctx);
        if let Some(array) = value.downcast_ref::<ArrayType>() {
            return Ok(
                ArrayType::get(ctx, self.storage_type(ctx, array.element(), &suffixes(0))?).into(),
            );
        }
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx).to_diag(ctx)?;
        let fields = record
            .deref(ctx)
            .fields()
            .to_diag(ctx)?
            .iter()
            .enumerate()
            .map(|(index, (name, field))| {
                Ok((
                    name.clone(),
                    self.storage_type(ctx, *field, &suffixes(index))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let kind = record.deref(ctx).kind();
        if let Some(class) = self.class_of_type(ctx, ty) {
            let projected = fresh_record(ctx, kind, Some(fields))?.into();
            self.class_records.borrow_mut().insert(projected, class);
            return Ok(projected);
        }
        Ok(self.mir_record_kind(ctx, kind, fields)?.into())
    }
}
