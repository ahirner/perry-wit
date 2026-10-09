//! Dense arrays and UTF-8 strings.
//!
//! Lengths are signless `i32`. Reads and writes take ECMAScript numbers as indices and follow
//! subset rule R8: reads miss with `undefined`, writes past `length` throw `RangeError`.
//! P3.5 narrows indices to `i32` and removes checks where ranges are proven.
use crate::{
    attrs::{ErrorKind, Tier},
    exceptions::MayThrowInterface,
    types::{ArrayType, OptionalType, StringType},
};
use pliron::{
    builtin::{
        attributes::StringAttr,
        op_interfaces::{
            NOpdsInterface, NResultsInterface, OneOpdInterface, OneResultInterface,
            SameOperandsAndResultType, SameOperandsType, SameResultsType,
        },
        types::{IntegerType, Signedness},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CollectionErr {
    #[error("{0} expects a perry.array")]
    NotArray(&'static str),
    #[error("{0} element type differs from the array's element type")]
    Element(&'static str),
    #[error("{0} index or length must be a signless i32")]
    Index(&'static str),
    #[error("{0} index must be a number (f64) or a signless i32")]
    NumberIndex(&'static str),
    #[error("{0} expects perry.string operands and result")]
    NotString(&'static str),
}

/// Signless `i32`, the type of lengths and indices.
pub fn index_type(ctx: &mut Context) -> TypeHandle {
    IntegerType::get(ctx, 32, Signedness::Signless).into()
}

fn is_index(ctx: &Context, value: Value) -> bool {
    value
        .get_type(ctx)
        .deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|ty| ty.width() == 32 && ty.signedness() == Signedness::Signless)
}

fn element(ctx: &Context, array: Value, op: &'static str) -> Result<TypeHandle> {
    match TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx) {
        Ok(array) => Ok(array.deref(ctx).element()),
        Err(_) => verify_err!(
            pliron::location::Location::Unknown,
            CollectionErr::NotArray(op)
        ),
    }
}

fn is_number(ctx: &Context, value: Value) -> bool {
    value
        .get_type(ctx)
        .deref(ctx)
        .is::<pliron::builtin::types::FP64Type>()
}

fn is_string(ctx: &Context, value: Value) -> bool {
    value.get_type(ctx).deref(ctx).is::<StringType>()
}

/// Allocate an array holding the operands in order.
#[pliron_op(
    name = "perry.array_new",
    format = "attr($perry_array_tier, $Tier) ` (` operands(CharSpace(`,`)) `) : ` type($0)",
    interfaces = [OneResultInterface],
    attributes = (perry_array_tier: Tier)
)]
pub struct ArrayNewOp;

impl ArrayNewOp {
    pub fn new(ctx: &mut Context, element: TypeHandle, items: Vec<Value>) -> Self {
        let ty = ArrayType::get(ctx, element).into();
        let op = ArrayNewOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                items,
                vec![],
                0,
            ),
        };
        op.set_attr_perry_array_tier(ctx, Tier::Unassigned);
        op
    }

    pub fn tier(&self, ctx: &Context) -> Tier {
        *self.get_attr_perry_array_tier(ctx).expect("array tier")
    }
}

impl Verify for ArrayNewOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let element = element(ctx, self.get_result(ctx), "perry.array_new")?;
        let op = self.get_operation().deref(ctx);
        if op.operands().any(|item| item.get_type(ctx) != element) {
            verify_err!(self.loc(ctx), CollectionErr::Element("perry.array_new"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for ArrayNewOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Number of elements.
#[pliron_op(
    name = "perry.array_len",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct ArrayLenOp;

impl ArrayLenOp {
    pub fn new(ctx: &mut Context, array: Value) -> Self {
        let ty = index_type(ctx);
        ArrayLenOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![array],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ArrayLenOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        element(
            ctx,
            self.get_operation().deref(ctx).get_operand(0),
            "perry.array_len",
        )?;
        if !is_index(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), CollectionErr::Index("perry.array_len"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for ArrayLenOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Read an element as `T | undefined`, as ECMAScript does: an index that is not an integer
/// in `[0, length)` yields `undefined` (arrays are dense, so in-range reads never miss).
/// The index is an ECMAScript number (`f64`) or a range-proven `i32` (P3.5); with a proven
/// in-bounds index P3.5 rewrites the result to the plain element type.
#[pliron_op(
    name = "perry.array_get",
    format = "$0 `[` $1 `] : ` type($0)",
    interfaces = [NOpdsInterface<2>, OneResultInterface]
)]
pub struct ArrayGetOp;

impl ArrayGetOp {
    pub fn new(ctx: &mut Context, array: Value, index: Value) -> Result<Self> {
        let element = element(ctx, array, "perry.array_get")?;
        let ty = OptionalType::get(ctx, element).into();
        Ok(ArrayGetOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![array, index],
                vec![],
                0,
            ),
        })
    }
}

impl Verify for ArrayGetOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        let element = element(ctx, op.get_operand(0), "perry.array_get")?;
        let result = self.get_result(ctx).get_type(ctx);
        let optional = TypedHandle::<OptionalType>::from_handle(result, ctx)
            .is_ok_and(|optional| optional.deref(ctx).value() == element);
        if !optional {
            verify_err!(self.loc(ctx), CollectionErr::Element("perry.array_get"))?
        }
        if !is_index(ctx, op.get_operand(1)) && !is_number(ctx, op.get_operand(1)) {
            verify_err!(self.loc(ctx), CollectionErr::NumberIndex("perry.array_get"))?
        }
        Ok(())
    }
}

/// Write an element at an index in `[0, length]`; writing at `length` appends.
/// Any other index throws a catchable `RangeError` instead of creating a hole (subset rule R8).
#[pliron_op(
    name = "perry.array_set",
    format = "$0 `[` $1 `] = ` $2",
    interfaces = [NOpdsInterface<3>, NResultsInterface<0>]
)]
pub struct ArraySetOp;

impl ArraySetOp {
    pub fn new(ctx: &mut Context, array: Value, index: Value, value: Value) -> Self {
        ArraySetOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![array, index, value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ArraySetOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if element(ctx, op.get_operand(0), "perry.array_set")? != op.get_operand(2).get_type(ctx) {
            verify_err!(self.loc(ctx), CollectionErr::Element("perry.array_set"))?
        }
        if !is_index(ctx, op.get_operand(1)) && !is_number(ctx, op.get_operand(1)) {
            verify_err!(self.loc(ctx), CollectionErr::NumberIndex("perry.array_set"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl MayThrowInterface for ArraySetOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::Any
    }
}

/// Append one element.
#[pliron_op(
    name = "perry.array_push",
    format = "$0 ` <- ` $1",
    interfaces = [NOpdsInterface<2>, NResultsInterface<0>]
)]
pub struct ArrayPushOp;

impl ArrayPushOp {
    pub fn new(ctx: &mut Context, array: Value, value: Value) -> Self {
        ArrayPushOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![array, value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ArrayPushOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if element(ctx, op.get_operand(0), "perry.array_push")? != op.get_operand(1).get_type(ctx) {
            verify_err!(self.loc(ctx), CollectionErr::Element("perry.array_push"))?
        }
        Ok(())
    }
}

/// A string literal; the text is validated UTF-8 by construction.
#[pliron_op(
    name = "perry.string_const",
    format = "attr($perry_string_value, $StringAttr) ` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface],
    attributes = (perry_string_value: StringAttr)
)]
pub struct StringConstOp;

impl StringConstOp {
    pub fn new(ctx: &mut Context, text: &str) -> Self {
        let ty = StringType::get(ctx).into();
        let op = StringConstOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_string_value(ctx, StringAttr::new(text.into()));
        op
    }
}

impl Verify for StringConstOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_string(ctx, self.get_result(ctx)) {
            verify_err!(
                self.loc(ctx),
                CollectionErr::NotString("perry.string_const")
            )?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for StringConstOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Concatenate two strings.
#[pliron_op(
    name = "perry.string_concat",
    format = "$0 `, ` $1 ` : ` type($0)",
    interfaces = [
        NOpdsInterface<2>,
        OneResultInterface,
        SameOperandsType,
        SameResultsType,
        SameOperandsAndResultType
    ]
)]
pub struct StringConcatOp;

impl StringConcatOp {
    pub fn new(ctx: &mut Context, lhs: Value, rhs: Value) -> Self {
        let ty = StringType::get(ctx).into();
        StringConcatOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![lhs, rhs],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for StringConcatOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_string(ctx, self.get_result(ctx)) {
            verify_err!(
                self.loc(ctx),
                CollectionErr::NotString("perry.string_concat")
            )?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for StringConcatOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Length in Unicode scalar values (Perry rejects UTF-16 code-unit operations).
#[pliron_op(
    name = "perry.string_len",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct StringLenOp;

impl StringLenOp {
    pub fn new(ctx: &mut Context, text: Value) -> Self {
        let ty = index_type(ctx);
        StringLenOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![text],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for StringLenOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let text = self.get_operation().deref(ctx).get_operand(0);
        if !is_string(ctx, text) {
            verify_err!(self.loc(ctx), CollectionErr::NotString("perry.string_len"))?
        }
        if !is_index(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), CollectionErr::Index("perry.string_len"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for StringLenOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

#[op_interface_impl]
impl MayThrowInterface for ArrayGetOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
#[op_interface_impl]
impl MayThrowInterface for ArrayPushOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
