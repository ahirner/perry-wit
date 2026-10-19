//! Promise construction, combination and settled-value projections.
use super::*;
use crate::operation_schema::op_schema;
use crate::signatures::{Accepts, OutputContract};
use crate::types::{PromiseType, SettledType};
use pliron::r#type::{TypeHandle, TypedHandle};

#[derive(Debug, Error)]
#[error("perry.promise_combinator takes an array of promises of one type")]
struct CombinatorErr;

/// How a [PromiseCombinatorOp] settles.
#[pliron::derive::pliron_attr(name = "perry.combinator", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Combinator {
    /// `Promise.all`: an array of every value, or the first rejection.
    All,
    /// `Promise.race`: the first settled input, in time and then input order.
    Race,
    /// `Promise.allSettled`: every input once all have settled, as `perry.settled` views.
    AllSettled,
}

op_schema! {
    /// `Promise.all`, `Promise.race` and `Promise.allSettled` of `perry.array<perry.promise<T>>`.
    pub PromiseCombinatorOp(promises) -> optional;
    name = "perry.promise_combinator";
    format = "attr($perry_combinator, $Combinator) ` ` $0 ` : ` type($0)";
    attributes = (perry_combinator: Combinator);
    constructors {
        /// `None` unless `promises` is an array of promises.
        pub new(combinator: Combinator, promises: Value) => infer(promises, combinator);
    }
    attribute(combinator: Combinator, get_attr_perry_combinator, set_attr_perry_combinator) = combinator => CombinatorErr;
    prepare(_this) { let combinator = *combinator; }
    result(ctx, promises: Value, combinator: Combinator): TypeHandle = {
        let element = TypedHandle::<ArrayType>::from_handle(promises.get_type(ctx), ctx)
            .ok()?
            .deref(ctx)
            .element();
        let value = TypedHandle::<PromiseType>::from_handle(element, ctx)
            .ok()?
            .deref(ctx)
            .value();
        let settled = match combinator {
            Combinator::All => ArrayType::get(ctx, value).into(),
            Combinator::Race => value,
            Combinator::AllSettled => {
                let settled = SettledType::get(ctx, value).into();
                ArrayType::get(ctx, settled).into()
            }
        };
        Some(PromiseType::get(ctx, settled).into())
    };
    error = CombinatorErr;
}

#[derive(Debug, Error)]
#[error("{0} has an invalid promise result type")]
struct PromiseResultErr(&'static str);

op_schema! {
    /// A promise fulfilled with its operand: a plain value among a combinator's inputs
    /// (`Promise.all([pending, 1])`).
    pub PromiseResolvedOp(value) -> infallible;
    name = "perry.promise_resolved";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(value); }
    result(ctx, value: Value): TypeHandle =
        PromiseType::get(ctx, value.get_type(ctx)).into();
    error = PromiseResultErr("perry.promise_resolved");
}

op_schema! {
    /// A promise rejected with its operand (`Promise.reject(e)`), of `perry.promise<T>` for the
    /// settled type `T` its context expects.
    pub PromiseRejectedOp(_reason) -> infallible;
    name = "perry.promise_rejected";
    format = "$0 ` : ` type($0)";
    constructors {
        pub new(_reason: Value, settled: TypeHandle)
            => explicit(PromiseType::get(ctx, settled).into());
    }
    result(ctx): impl OutputContract = Accepts(|ctx: &Context, ty: TypeHandle| {
        ty.deref(ctx).is::<PromiseType>()
    });
    error = PromiseResultErr("perry.promise_rejected");
}

#[derive(Debug, Error)]
#[error("perry.settled_field reads a perry.settled value")]
struct SettledErr;

/// A property of a `Promise.allSettled` result element.
#[pliron::derive::pliron_attr(name = "perry.settled_field", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum SettledField {
    /// `"fulfilled"` or `"rejected"`.
    Status,
    /// The fulfillment value, or `undefined`.
    Value,
    /// The rejection (an exception), or `undefined`.
    Reason,
}

op_schema! {
    /// `settled.status`, `settled.value` or `settled.reason`.
    pub SettledFieldOp(settled) -> optional;
    name = "perry.settled_field";
    format = "attr($perry_settled_field, $SettledField) ` ` $0 ` : ` type($0)";
    attributes = (perry_settled_field: SettledField);
    constructors {
        /// `None` unless `settled` is a `perry.settled` value.
        pub new(field: SettledField, settled: Value) => infer(settled, field);
    }
    attribute(field: SettledField, get_attr_perry_settled_field, set_attr_perry_settled_field) = field => SettledErr;
    prepare(_this) { let field = *field; }
    result(ctx, settled: Value, field: SettledField): TypeHandle = {
        let value = TypedHandle::<SettledType>::from_handle(settled.get_type(ctx), ctx)
            .ok()?
            .deref(ctx)
            .value();
        Some(match field {
            SettledField::Status => StringType::get(ctx).into(),
            SettledField::Value => OptionalType::get(ctx, value).into(),
            SettledField::Reason => {
                let exception = ExceptionType::get(ctx).into();
                OptionalType::get(ctx, exception).into()
            }
        })
    };
    error = SettledErr;
}
