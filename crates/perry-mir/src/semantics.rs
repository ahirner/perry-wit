//! Primitive semantics shared by runtime capabilities and value/heap transfer.
use crate::{
    attrs::ErrorKind,
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp},
    collections::{
        ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
        ArrayTruncateOp, ConcatOp, ConcatReuseOp, ConcatSegmentsOp, JsonBoxOp, NativeJsonOp,
        StringConstOp, StringLenOp,
    },
    control::ReturnOp,
    effects::{Allocation, Mutation},
    exceptions::{
        CatchOp, CaughtOp, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExitOp, RaiseOp,
        RethrowOp,
    },
    foreign::{ForeignOp, Phase},
    globals::{GlobalLoadOp, GlobalOp},
    heap::{AllocOp, FieldGetOp, FieldPresentOp, FieldSetOp, RecordOrderOp},
    integers::BigIntConstOp,
    intrinsics::{
        Combinator, Intrinsic, IntrinsicOp, JsonCopyOp, JsonDecodeOp, JsonStringifyOp,
        PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp, SettledFieldOp,
    },
    locals::{LoadOp, StoreOp, UndefOp},
    optional::{
        IsNullOp, IsUndefinedOp, NullOp, OmittedOp, OptionalCastOp, ToNumberOp, ToStringOp,
        UndefinedOp, UnwrapOp, WrapOp,
    },
    regex::RegexConstOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::{BigIntType, RecordType},
    variants::{VariantNewOp, VariantPayloadOp, VariantTagOp},
    views::AggregateViewOp,
};
use pliron::{
    builtin::{attributes::StringAttr, op_interfaces::BranchOpInterface},
    context::{Context, Ptr},
    derive::{op_interface, op_interface_impl},
    op::{Op, op_cast},
    operation::Operation,
    result::Result,
    r#type::Typed,
    value::Value,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Intrinsic(Intrinsic),
    Call { asynchronous: bool },
    Await,
    Timer,
    Foreign(Phase),
    Mutate(Mutation),
    Allocate { array: bool },
    Read { slot: usize },
    Length,
    Combine(Combinator),
    Concat { depth: u8 },
    Alias,
    Resolved,
    Rejected,
    Return,
    Raise,
    Stringify,
    JsonDecode,
    NumericCast,
    Unknown,
    None,
}

impl OperationKind {
    pub(crate) fn classify(ctx: &Context, op: Ptr<Operation>) -> Self {
        let dynamic = Operation::get_op_dyn(op, ctx);
        if let Some(semantics) = op_cast::<dyn PrimitiveSemantics>(&*dynamic) {
            semantics.kind(ctx)
        } else if op_cast::<dyn BranchOpInterface>(&*dynamic).is_some() {
            Self::None
        } else {
            Self::Unknown
        }
    }

    pub(crate) fn promise_payload(self) -> Option<usize> {
        match self {
            Self::Resolved => Some(0),
            Self::Timer => Some(1),
            _ => None,
        }
    }
}

#[op_interface]
pub(crate) trait PrimitiveSemantics {
    fn kind(&self, ctx: &Context) -> OperationKind;
    fn allocation(&self, ctx: &Context) -> Allocation;
    fn exception(&self, ctx: &Context) -> Option<ErrorKind>;

    fn verify(_op: &dyn Op, _ctx: &Context) -> Result<()>
    where
        Self: Sized,
    {
        Ok(())
    }
}

pub(crate) fn exception(ctx: &Context, op: Ptr<Operation>) -> Option<ErrorKind> {
    op_cast::<dyn PrimitiveSemantics>(Operation::get_op_dyn(op, ctx).as_ref())?.exception(ctx)
}

macro_rules! primitive_semantics {
    ($ctx:ident, $this:ident; $(($allocation:expr, $exception:expr, $effect:expr) {
        $($($op:ty),+ => $kind:expr;)*
    })*) => {$($($(
        #[op_interface_impl]
        impl PrimitiveSemantics for $op {
            fn kind(&self, $ctx: &Context) -> OperationKind {
                let $this = self;
                $kind
            }
            fn allocation(&self, $ctx: &Context) -> Allocation {
                let $this = self;
                $allocation
            }
            fn exception(&self, $ctx: &Context) -> Option<ErrorKind> {
                let $this = self;
                $exception
            }
        }
        #[op_interface_impl]
        impl pliron::opts::dce::SideEffects for $op {
            fn has_side_effects(&self, $ctx: &Context) -> bool {
                let $this = self;
                $effect
            }
        }
    )*)*)*};
}

primitive_semantics! { _ctx, _this;
    // Every intrinsic supports a status boundary; the service policy decides whether it throws.
    (
        _this.intrinsic(_ctx).policy().allocation,
        Some(ErrorKind::Any),
        _this.intrinsic(_ctx).policy().effect != crate::intrinsics::ServiceEffect::Pure
    ) {
        IntrinsicOp => OperationKind::Intrinsic(_this.intrinsic(_ctx));
    }
    (Allocation::None, Some(ErrorKind::Any), true) {
        CallOp => OperationKind::Call { asynchronous: false };
        AwaitOp => OperationKind::Await;
        RethrowOp => OperationKind::Unknown;
    }
    (Allocation::None, None, true) {
        AsyncCallOp => OperationKind::Call { asynchronous: true };
        ArrayTruncateOp => OperationKind::Mutate(Mutation::Truncate);
        StoreOp => OperationKind::Mutate(Mutation::Local);
        ReturnOp => OperationKind::Return;
        CaughtOp, ExitOp => OperationKind::Unknown;
    }
    (Allocation::Conservative, Some(ErrorKind::TypeError), true) {
        ForeignOp => OperationKind::Foreign(_this.phase(_ctx));
        JsonStringifyOp => OperationKind::Stringify;
    }
    (Allocation::Conservative, None, true) {
        TimerOp => OperationKind::Timer;
        PromiseResolvedOp => OperationKind::Resolved;
        PromiseRejectedOp => OperationKind::Rejected;
        PromiseCombinatorOp => OperationKind::Combine(_this.combinator(_ctx));
        RaiseOp => OperationKind::Raise;
        CatchOp, RecordOrderOp => OperationKind::Unknown;
    }
    (Allocation::None, None, false) {
        WrapOp, ToNumberOp => OperationKind::Alias;
        FieldGetOp => field_slot(_ctx, _this.get_operation().deref(_ctx).get_operand(0), _this.get_attr_perry_get_field(_ctx).as_deref())
            .map_or(OperationKind::Unknown, |slot| OperationKind::Read { slot });
        ArrayGetOp, ArrayAtOp => OperationKind::Read { slot: 0 };
        ArrayLenOp, StringLenOp => OperationKind::Length;
        UndefinedOp, OmittedOp, NullOp, FieldPresentOp, IsNullOp, IsUndefinedOp => OperationKind::None;
        AggregateViewOp, BigIntConstOp, CompareOp, ExceptionIsOp, GlobalOp,
        LoadOp, RegexConstOp, SettledFieldOp, StringConstOp, ToStringOp, UndefOp,
        VariantTagOp => OperationKind::Unknown;
    }
    (Allocation::None, Some(ErrorKind::TypeError), true) {
        UnwrapOp, OptionalCastOp => OperationKind::Alias;
        VariantPayloadOp, GlobalLoadOp => OperationKind::Unknown;
    }
    (Allocation::Atomic, None, false) {
        AllocOp => OperationKind::Allocate { array: false };
        ArrayNewOp => OperationKind::Allocate { array: true };
        ConcatOp => OperationKind::Concat { depth: 1 };
        ConcatSegmentsOp => OperationKind::Concat { depth: 2 };
        NativeJsonOp, JsonBoxOp, VariantNewOp => OperationKind::Unknown;
    }
    (if crate::field_order_may_change(
        _ctx, _this.get_operation().deref(_ctx).get_operand(1).get_type(_ctx)
    ) { Allocation::Atomic } else { Allocation::None }, None, true) {
        FieldSetOp => OperationKind::Mutate(Mutation::Field { slot:
            field_slot(_ctx, _this.get_operation().deref(_ctx).get_operand(0), _this.get_attr_perry_set_field(_ctx).as_deref()) });
    }
    (Allocation::Atomic, Some(ErrorKind::RangeError), true) {
        ArraySetOp => OperationKind::Mutate(Mutation::Element { index: Some(1) });
    }
    (Allocation::Atomic, None, true) {
        ArrayPushOp => OperationKind::Mutate(Mutation::Element { index: None });
        ArrayExtendOp => OperationKind::Mutate(Mutation::Extend);
        ConcatReuseOp => OperationKind::Concat { depth: 1 };
    }
    (Allocation::Atomic, Some(ErrorKind::Any), true) {
        JsonCopyOp, JsonDecodeOp => OperationKind::JsonDecode;
    }
    (if bigint_result(_ctx, _this) { Allocation::Atomic } else { Allocation::None }, None, false) {
        NumericCastOp => OperationKind::NumericCast;
    }
    (if bigint_result(_ctx, _this) { Allocation::Atomic } else { Allocation::None },
    if bigint_result(_ctx, _this) { Some(ErrorKind::RangeError) } else { None }, false) {
        BinaryOp => OperationKind::Unknown;
    }
    (Allocation::Conservative, None, false) {
        ErrorNewOp, ExceptionMessageOp => OperationKind::Unknown;
    }
}

fn bigint_result(ctx: &Context, op: &impl Op) -> bool {
    op.get_operation()
        .deref(ctx)
        .get_result(0)
        .get_type(ctx)
        .deref(ctx)
        .is::<BigIntType>()
}

fn field_slot(ctx: &Context, record: Value, field: Option<&StringAttr>) -> Option<usize> {
    record
        .get_type(ctx)
        .deref(ctx)
        .downcast_ref::<RecordType>()?
        .field(field?.as_str())
        .map(|(slot, _)| slot)
}
