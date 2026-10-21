//! Dense arrays and UTF-8 strings.
//!
//! Lengths are signless `i32`. Reads and writes take ECMAScript numbers as indices: out-of-bounds
//! reads yield `undefined` and writes past `length` throw `RangeError`.
//! Range analysis narrows indices to `i32` and elides bounds checks where ranges are proven.
use crate::operation_schema::op_schema;
use crate::signatures::{OutputContract, ResultType, Signature, Ty};
use crate::{
    attrs::{ReuseOperands, Tier},
    types::{ArrayType, OptionalType, StringType},
};
use pliron::{
    builtin::{
        attributes::StringAttr,
        op_interfaces::{
            OneResultInterface, SameOperandsAndResultType, SameOperandsType, SameResultsType,
        },
    },
    common_traits::Verify,
    context::Context,
    op::Op,
    operation::Operation,
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

fn element(ctx: &Context, array: Value, op: &'static str) -> Result<TypeHandle> {
    match TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx) {
        Ok(array) => Ok(array.deref(ctx).element()),
        Err(_) => verify_err!(
            pliron::location::Location::Unknown,
            CollectionErr::NotArray(op)
        ),
    }
}

fn sequence_type(ctx: &Context, sources: &[Value]) -> TypeHandle {
    sources
        .first()
        .map_or_else(|| StringType::get(ctx).into(), |v| v.get_type(ctx))
}

struct Sequence;
impl OutputContract for Sequence {
    fn accepts(self, ctx: &Context, op: &Operation) -> bool {
        let Some(ty) = crate::signatures::result_types(ctx, op).next() else {
            return false;
        };
        let count = op.get_num_operands();
        let string = Ty::String.matches(ctx, ty);
        op.get_num_results() == 1
            && count > 0
            && (string || ty.deref(ctx).is::<ArrayType>())
            && (!string || count == 2)
    }
}

op_schema! {
    /// Allocate an array holding the operands in order.
    pub ArrayNewOp(..items) -> infallible;
    name = "perry.array_new";
    format = "attr($perry_array_tier, $Tier) ` (` operands(CharSpace(`,`)) `) : ` type($0)";
    attributes = (perry_array_tier: Tier);
    constructors {
        pub new(element: TypeHandle, items: Vec<Value>) operands(items)
            => explicit(ArrayType::get(ctx, element).into());
    }
    attribute(_tier, get_attr_perry_array_tier, set_attr_perry_array_tier) = Tier::Unassigned;
    prepare(this) {
        <Self as OneResultInterface>::verify(this, ctx)?;
        let element = element(ctx, this.get_result(ctx), "perry.array_new")?;
        let count = items.get_num_operands();
    }
    result(ctx, element: TypeHandle, count: usize): impl OutputContract =
        Signature {
            inputs: std::iter::repeat_n(element, count),
            outputs: [TypeHandle::from(ArrayType::get(ctx, element))],
        };
    error = CollectionErr::Element("perry.array_new");
}

op_schema! {
    /// Number of elements.
    pub ArrayLenOp(array) -> infallible;
    name = "perry.array_len";
    format = "$0 ` : ` type($0)";
    constructors { pub new(array: Value) => infer(); }
    result(ctx): Ty = Ty::Index;
    error = CollectionErr::Index("perry.array_len");
    require element(ctx, array, "perry.array_len")?;
}

op_schema! {
    /// Read an element as `T | undefined`: an index in `[0, length)` yields the element, and
    /// any other index yields `undefined`. Accepts an ECMAScript number (`f64`) or a
    /// range-proven `i32`.
    pub ArrayGetOp(array, index) -> fallible;
    name = "perry.array_get";
    format = "$0 `[` $1 `] : ` type($0)";
    constructors { pub new(array: Value, index: Value) => infer(array); }
    result(ctx, array: Value): TypeHandle = {
        let element = element(ctx, array, "perry.array_get")?;
        Ok(OptionalType::get(ctx, element).into())
    };
    error = CollectionErr::Element("perry.array_get");
    check Ty::Index.matches(ctx, index.get_type(ctx))
        || Ty::Number.matches(ctx, index.get_type(ctx))
        => CollectionErr::NumberIndex("perry.array_get");
}

op_schema! {
    /// Unchecked element read at an `i32` index proven in `[0, length)` by `perry_mir::ranges`.
    pub ArrayAtOp(array, index) -> fallible;
    name = "perry.array_at";
    format = "$0 `[` $1 `] : ` type($0)";
    constructors { pub new(array: Value, index: Value) => infer(array); }
    result(ctx, array: Value): TypeHandle = element(ctx, array, "perry.array_at");
    error = CollectionErr::Element("perry.array_at");
    check Ty::Index.matches(ctx, index.get_type(ctx)) => CollectionErr::Index("perry.array_at");
}

op_schema! {
    /// Write an element at an index in `[0, length]`. Writing at `length` appends to the array,
    /// and any out-of-range index throws a catchable `RangeError`.
    pub ArraySetOp(array, index, value) -> infallible;
    name = "perry.array_set";
    format = "$0 `[` $1 `] = ` $2";
    constructors { pub new(array: Value, index: Value, value: Value) => empty(); }
    result(ctx): () = ();
    error = CollectionErr::Element("perry.array_set");
    check_before element(ctx, array, "perry.array_set")? == value.get_type(ctx)
        => CollectionErr::Element("perry.array_set");
    check Ty::Index.matches(ctx, index.get_type(ctx)) || Ty::Number.matches(ctx, index.get_type(ctx))
        => CollectionErr::NumberIndex("perry.array_set");
}

op_schema! {
    /// Append one element.
    pub ArrayPushOp(array, value) -> infallible;
    name = "perry.array_push";
    format = "$0 ` <- ` $1";
    constructors { pub new(array: Value, value: Value) => empty(); }
    result(ctx): () = ();
    error = CollectionErr::Element("perry.array_push");
    check_before element(ctx, array, "perry.array_push")? == value.get_type(ctx)
        => CollectionErr::Element("perry.array_push");
}

op_schema! {
    /// Materialize a string literal whose text is validated UTF-8 by construction.
    pub StringConstOp() -> infallible;
    name = "perry.string_const";
    format = "attr($perry_string_value, $StringAttr) ` : ` type($0)";
    attributes = (perry_string_value: StringAttr);
    constructors { pub new(text: &str) => infer(); }
    attribute(_value, get_attr_perry_string_value, set_attr_perry_string_value) = StringAttr::new(text.into());
    result(ctx): Ty = Ty::String;
    error = CollectionErr::NotString("perry.string_const");
}

op_schema! {
    /// Extend a mutable array with a snapshot of another array's slots.
    pub ArrayExtendOp(target, _source) -> infallible;
    name = "perry.array_extend";
    format = "$0 `, ` $1";
    interfaces = [SameOperandsType];
    constructors { pub new(target: Value, _source: Value) => empty(); }
    result(ctx): () = ();
    error = CollectionErr::NotArray("perry.array_extend");
    require element(ctx, target, "perry.array_extend")?;
}

op_schema! {
    /// Shrink an array's length in place.
    pub ArrayTruncateOp(array, new_len) -> infallible;
    name = "perry.array_truncate";
    format = "$0 `, ` $1";
    constructors { pub new(array: Value, new_len: Value) => empty(); }
    result(ctx): () = ();
    error = CollectionErr::Index("perry.array_truncate");
    require element(ctx, array, "perry.array_truncate")?;
    check Ty::Index.matches(ctx, new_len.get_type(ctx)) => CollectionErr::Index("perry.array_truncate");
}

op_schema! {
    /// A dynamic view of a native aggregate, retaining its mutable identity and typed layout.
    pub NativeJsonOp(value) -> infallible;
    name = "perry.json_native";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(); }
    result(ctx): Ty = Ty::Json;
    error = CollectionErr::Sequence("perry.json_native");
    check_before crate::types::is_reference(ctx, value.get_type(ctx))
        => CollectionErr::Sequence("perry.json_native");
}

op_schema! {
    /// A dynamic JSON value backed by its original native storage.
    pub JsonBoxOp(_value) -> infallible;
    name = "perry.json_box";
    format = "$0 ` : ` type($0)";
    constructors { pub new(_value: Value) => infer(); }
    result(ctx): Ty = Ty::Json;
    error = CollectionErr::Sequence("perry.json_box");
}

op_schema! {
    /// Dynamic argument spreads normalize to a sequence of typed dense segments.
    pub ConcatSegmentsOp(sources) -> fallible;
    name = "perry.concat_segments";
    format = "$0 ` : ` type($0)";
    constructors {
        pub new(sources: Value) -> infallible => explicit(
            Self::result_contract(ctx, sources).unwrap_or_else(|_| sources.get_type(ctx))
        );
    }
    prepare(this) {}
    result(ctx, sources: Value): TypeHandle = element(ctx, sources, "perry.concat_segments");
    error = CollectionErr::Sequence("perry.concat_segments");
    check this.get_result(ctx).get_type(ctx).deref(ctx).is::<ArrayType>()
        => CollectionErr::Sequence("perry.concat_segments");
}

op_schema! {
    /// Combine strings or dense arrays in operand order.
    pub ConcatOp(.._sources) -> infallible;
    name = "perry.concat";
    format = "operands(CharSpace(`,`)) ` : ` type($0)";
    interfaces = [
        SameOperandsType,
        SameResultsType,
        SameOperandsAndResultType,
    ];
    constructors {
        pub new(lhs: Value, rhs: Value) operands(vec![lhs, rhs]) => explicit(lhs.get_type(ctx));
        pub from_sources(sources: Vec<Value>) operands(sources) => explicit(sequence_type(ctx, &sources));
    }
    result(ctx): Sequence = Sequence;
    error = CollectionErr::Sequence("perry.concat");
}

op_schema! {
    /// Concatenate sequences with verified exclusive operands eligible for storage reuse.
    pub ConcatReuseOp(..sources) -> infallible;
    name = "perry.concat_reuse";
    format = "attr($perry_concat_reuse, $ReuseOperands) ` ` operands(CharSpace(`,`)) ` : ` type($0)";
    interfaces = [
        SameOperandsType,
        SameResultsType,
        SameOperandsAndResultType,
    ];
    attributes = (perry_concat_reuse: ReuseOperands);
    constructors {
        pub(crate) from_sources(sources: Vec<Value>, reuse: ReuseOperands) operands(sources)
            => explicit(sequence_type(ctx, &sources));
    }
    attribute(candidates: ReuseOperands, get_attr_perry_concat_reuse, set_attr_perry_concat_reuse) = reuse
        => CollectionErr::Sequence("perry.concat_reuse");
    result(ctx): Sequence = Sequence;
    error = CollectionErr::Sequence("perry.concat_reuse");
    check !candidates.0.is_empty()
        && candidates.0.windows(2).all(|pair| pair[0] < pair[1])
        && (!Ty::String.matches(ctx, sources.get_result(0).get_type(ctx)) || candidates.0 == [0])
        && candidates.0.iter().all(|&index| (index as usize) < sources.get_num_operands()
            && sources.operands().filter(|v| *v == sources.get_operand(index as usize)).count() == 1)
        => CollectionErr::Sequence("perry.concat_reuse");
}

op_schema! {
    /// String length in Unicode scalar values.
    pub StringLenOp(text) -> infallible;
    name = "perry.string_len";
    format = "$0 ` : ` type($0)";
    constructors { pub new(text: Value) => infer(); }
    result(ctx): Ty = Ty::Index;
    error = CollectionErr::Index("perry.string_len");
    check_before Ty::String.matches(ctx, text.get_type(ctx))
        => CollectionErr::NotString("perry.string_len");
}
