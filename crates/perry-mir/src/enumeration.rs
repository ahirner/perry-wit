//! Own-key, value and entry snapshots with statically typed value slots.
use crate::{
    attrs::ErrorKind,
    exceptions::MayThrowInterface,
    optional::optional_value,
    types::{
        ArrayType, BufferType, BytesType, DictionaryType, JsonType, OptionalType, RecordType,
        StringType, TupleType,
    },
};
use pliron::{
    builtin::{
        op_interfaces::{OneOpdInterface, OneResultInterface},
        types::{FP64Type, IntegerType},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_attr, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};

pub fn common_type(
    ctx: &Context,
    elements: impl Iterator<Item = TypeHandle>,
) -> Option<TypeHandle> {
    let elements: Vec<_> = elements.collect();
    let first = elements
        .first()
        .copied()
        .unwrap_or_else(|| JsonType::get(ctx).into());
    let base = optional_value(ctx, first).unwrap_or(first);
    let ty = if elements
        .iter()
        .all(|&ty| optional_value(ctx, ty).unwrap_or(ty) == base)
    {
        base
    } else if elements.iter().all(|&ty| crate::json::compatible(ctx, ty)) {
        JsonType::get(ctx).into()
    } else {
        return None;
    };
    Some(
        if elements.iter().any(|&ty| optional_value(ctx, ty).is_some()) {
            OptionalType::get(ctx, ty).into()
        } else {
            ty
        },
    )
}

pub fn value_type(ctx: &Context, source: TypeHandle) -> Option<TypeHandle> {
    let source = optional_value(ctx, source).unwrap_or(source);
    let ty = source.deref(ctx);
    if let Some(dictionary) = ty.downcast_ref::<DictionaryType>() {
        return dictionary.element();
    }
    if let Some(array) = ty.downcast_ref::<ArrayType>() {
        return Some(array.element());
    }
    if let Some(record) = ty.downcast_ref::<RecordType>() {
        return common_type(ctx, record.fields().ok()?.iter().map(|field| field.ty));
    }
    if let Some(tuple) = ty.downcast_ref::<TupleType>() {
        return common_type(ctx, tuple.elements().iter().copied());
    }
    if ty.is::<StringType>() {
        return Some(source);
    }
    if ty.is::<BytesType>() {
        return Some(FP64Type::get(ctx).into());
    }
    if ty.is::<JsonType>() {
        return Some(OptionalType::get(ctx, JsonType::get(ctx).into()).into());
    }
    if ty.is::<FP64Type>()
        || ty.is::<BufferType>()
        || ty
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() == 1)
    {
        return Some(JsonType::get(ctx).into());
    }
    None
}

#[pliron_attr(name = "perry.enumeration_action", format, verifier = "succ")]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    Keys,
    Values,
    Entries,
}
impl Action {
    pub fn result(self, ctx: &Context, source: TypeHandle) -> Option<TypeHandle> {
        let value = if self == Self::Keys {
            let source = optional_value(ctx, source).unwrap_or(source);
            let ty = source.deref(ctx);
            if !ty.is::<RecordType>() && !ty.is::<TupleType>() && value_type(ctx, source).is_none()
            {
                return None;
            }
            StringType::get(ctx).into()
        } else {
            let value = value_type(ctx, source)?;
            if self == Self::Entries {
                TupleType::get(ctx, vec![StringType::get(ctx).into(), value]).into()
            } else {
                value
            }
        };
        Some(ArrayType::get(ctx, value).into())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("enumeration requires supported source and matching snapshot element types")]
struct SignatureError;

#[pliron_op(name = "perry.enumerate", format = "attr($perry_enumeration_action, $Action) ` (` $0 `) : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface], attributes = (perry_enumeration_action: Action))]
pub struct EnumerateOp;
impl EnumerateOp {
    pub fn new(ctx: &mut Context, action: Action, source: Value) -> Result<Self> {
        let Some(result) = action.result(ctx, source.get_type(ctx)) else {
            return verify_err!(pliron::location::Location::Unknown, SignatureError);
        };
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![source],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_enumeration_action(ctx, action);
        Ok(op)
    }
    pub fn action(&self, ctx: &Context) -> Action {
        *self
            .get_attr_perry_enumeration_action(ctx)
            .expect("enumeration action")
    }
}
impl Verify for EnumerateOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if self
            .action(ctx)
            .result(ctx, self.get_operand(ctx).get_type(ctx))
            != Some(self.get_result(ctx).get_type(ctx))
        {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for EnumerateOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        true
    }
}
#[op_interface_impl]
impl MayThrowInterface for EnumerateOp {
    fn exception(&self, _: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
