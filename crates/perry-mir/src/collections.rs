//! Dense arrays and UTF-8 strings.
//!
//! Lengths are signless `i32`. Reads and writes take ECMAScript numbers as indices and follow
//! subset rule R8: reads miss with `undefined`, writes past `length` throw `RangeError`.
//! P3.5 narrows indices to `i32` and removes checks where ranges are proven.
use crate::{
    attrs::{ErrorKind, ReuseOperands, Tier},
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
enum CollectionErr {
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
    #[error("{0} requires compatible sequence operands and valid exclusive reuse candidates")]
    Sequence(&'static str),
}

/// Signless `i32`, the type of lengths and indices.
pub(crate) fn index_type(ctx: &mut Context) -> TypeHandle {
    IntegerType::get(ctx, 32, Signedness::Signless).into()
}

fn is_index(ctx: &Context, value: Value) -> bool {
    value
        .get_type(ctx)
        .deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|ty| ty.width() == 32)
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

fn is_sequence(ctx: &Context, value: Value) -> bool {
    let ty = value.get_type(ctx).deref(ctx);
    ty.is::<StringType>() || ty.is::<ArrayType>()
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
        let optional = result == OptionalType::get(ctx, element).into();
        if !optional {
            verify_err!(self.loc(ctx), CollectionErr::Element("perry.array_get"))?
        }
        if !is_index(ctx, op.get_operand(1)) && !is_number(ctx, op.get_operand(1)) {
            verify_err!(self.loc(ctx), CollectionErr::NumberIndex("perry.array_get"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for ArrayGetOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Read an element at an `i32` index proven in `[0, length)` (P3.5, `perry_mir::ranges`):
/// the plain element, with no check. Only the range pass creates it.
#[pliron_op(
    name = "perry.array_at",
    format = "$0 `[` $1 `] : ` type($0)",
    interfaces = [NOpdsInterface<2>, OneResultInterface]
)]
pub struct ArrayAtOp;

impl ArrayAtOp {
    pub fn new(ctx: &mut Context, array: Value, index: Value) -> Result<Self> {
        let element = element(ctx, array, "perry.array_at")?;
        Ok(ArrayAtOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![element],
                vec![array, index],
                vec![],
                0,
            ),
        })
    }
}

impl Verify for ArrayAtOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if element(ctx, op.get_operand(0), "perry.array_at")? != self.get_result(ctx).get_type(ctx)
        {
            verify_err!(self.loc(ctx), CollectionErr::Element("perry.array_at"))?
        }
        if !is_index(ctx, op.get_operand(1)) {
            verify_err!(self.loc(ctx), CollectionErr::Index("perry.array_at"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for ArrayAtOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
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
        ErrorKind::RangeError
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

/// Extend a mutable array with a snapshot of another array's slots.
#[pliron_op(name = "perry.array_extend", format = "$0 `, ` $1", interfaces = [NOpdsInterface<2>, NResultsInterface<0>, SameOperandsType])]
pub struct ArrayExtendOp;
impl ArrayExtendOp {
    pub fn new(ctx: &mut Context, target: Value, source: Value) -> Self {
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![target, source],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for ArrayExtendOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        element(
            ctx,
            self.get_operation().deref(ctx).get_operand(0),
            "perry.array_extend",
        )?;
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for ArrayExtendOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        true
    }
}

/// Shrink an array's length in-place without reallocating.
#[pliron_op(
    name = "perry.array_truncate",
    format = "$0 `, ` $1",
    interfaces = [NOpdsInterface<2>, NResultsInterface<0>]
)]
pub struct ArrayTruncateOp;

impl ArrayTruncateOp {
    pub fn new(ctx: &mut Context, array: Value, new_len: Value) -> Self {
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![array, new_len],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ArrayTruncateOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        element(ctx, op.get_operand(0), "perry.array_truncate")?;
        if !is_index(ctx, op.get_operand(1)) {
            verify_err!(self.loc(ctx), CollectionErr::Index("perry.array_truncate"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for ArrayTruncateOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        true
    }
}

/// A dynamic view of a native aggregate, retaining its mutable identity and typed layout.
#[pliron_op(name = "perry.json_native", format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
pub struct NativeJsonOp;
impl NativeJsonOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let json = crate::types::JsonType::get(ctx).into();
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![json],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for NativeJsonOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !crate::types::is_reference(ctx, self.get_operand(ctx).get_type(ctx)) {
            verify_err!(self.loc(ctx), CollectionErr::Sequence("perry.json_native"))?
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for NativeJsonOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}

/// Dynamic argument spreads normalize to a sequence of typed dense segments.
#[pliron_op(name = "perry.concat_segments", format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
pub struct ConcatSegmentsOp;
impl ConcatSegmentsOp {
    pub fn new(ctx: &mut Context, sources: Value) -> Self {
        let ty = element(ctx, sources, "perry.concat_segments").expect("typed segment array");
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![sources],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for ConcatSegmentsOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let source = element(ctx, self.get_operand(ctx), "perry.concat_segments")?;
        if !source.deref(ctx).is::<ArrayType>() || source != self.get_result(ctx).get_type(ctx) {
            verify_err!(
                self.loc(ctx),
                CollectionErr::Sequence("perry.concat_segments")
            )?
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for ConcatSegmentsOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}

/// Combine strings or dense arrays in operand order.
#[pliron_op(
    name = "perry.concat",
    format = "operands(CharSpace(`,`)) ` : ` type($0)",
    interfaces = [
        OneResultInterface,
        SameOperandsType,
        SameResultsType,
        SameOperandsAndResultType
    ]
)]
pub struct ConcatOp;

impl ConcatOp {
    pub fn new(ctx: &mut Context, lhs: Value, rhs: Value) -> Self {
        Self::from_sources(ctx, vec![lhs, rhs])
    }

    pub fn from_sources(ctx: &mut Context, sources: Vec<Value>) -> Self {
        let ty = sources[0].get_type(ctx);
        ConcatOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                sources,
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ConcatOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let count = self.get_operation().deref(ctx).get_num_operands();
        if !is_sequence(ctx, self.get_result(ctx))
            || count == 0
            || (is_string(ctx, self.get_result(ctx)) && count != 2)
        {
            verify_err!(self.loc(ctx), CollectionErr::Sequence("perry.concat"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for ConcatOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Concatenate sequences with verified exclusive operands eligible for storage reuse.
#[pliron_op(
    name = "perry.concat_reuse",
    format = "attr($perry_concat_reuse, $ReuseOperands) ` ` operands(CharSpace(`,`)) ` : ` type($0)",
    attributes = (perry_concat_reuse: ReuseOperands),
    interfaces = [
        OneResultInterface,
        SameOperandsType,
        SameResultsType,
        SameOperandsAndResultType
    ]
)]
pub struct ConcatReuseOp;

impl ConcatReuseOp {
    pub(crate) fn from_sources(
        ctx: &mut Context,
        sources: Vec<Value>,
        reuse: ReuseOperands,
    ) -> Self {
        let ty = sources[0].get_type(ctx);
        let operation = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                sources,
                vec![],
                0,
            ),
        };
        operation.set_attr_perry_concat_reuse(ctx, reuse);
        operation
    }

    pub fn candidates(&self, ctx: &Context) -> ReuseOperands {
        self.get_attr_perry_concat_reuse(ctx)
            .expect("concat reuse candidates")
            .clone()
    }
}

impl Verify for ConcatReuseOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let operation = self.get_operation().deref(ctx);
        let operands: Vec<_> = operation.operands().collect();
        let reuse = self.get_attr_perry_concat_reuse(ctx);
        let candidates = reuse.as_deref().map_or(&[][..], |reuse| reuse.0.as_slice());
        if !is_sequence(ctx, self.get_result(ctx))
            || operands.is_empty()
            || candidates.is_empty()
            || candidates.windows(2).any(|pair| pair[0] >= pair[1])
            || (is_string(ctx, self.get_result(ctx)) && (operands.len() != 2 || candidates != [0]))
            || candidates.iter().any(|&index| {
                operands.get(index as usize).is_none_or(|candidate| {
                    operands.iter().filter(|value| *value == candidate).count() != 1
                })
            })
        {
            verify_err!(self.loc(ctx), CollectionErr::Sequence("perry.concat_reuse"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for ConcatReuseOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        true
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
