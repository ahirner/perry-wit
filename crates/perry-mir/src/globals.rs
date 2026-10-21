//! Typed module cells. Local slots promote to SSA values, while module cells persist across export calls.
use crate::operation_schema::op_schema;
use crate::types::SlotType;
use pliron::{
    builtin::{attributes::IdentifierAttr, op_interfaces::OneResultInterface},
    common_traits::Verify,
    context::Context,
    identifier::Identifier,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};

pub const INITIALIZER: &str = "perry_module_init";

op_schema! {
    /// Address of a statically typed persistent cell, initialized by the module initializer.
    pub GlobalOp() -> fallible;
    name = "perry.global";
    format = "attr($perry_global, $IdentifierAttr) ` : ` type($0)";
    attributes = (perry_global: IdentifierAttr);
    constructors {
        pub new(name: Identifier, value: TypeHandle) -> infallible
            => explicit(SlotType::get(ctx, value).into());
    }
    attribute(name: Identifier, get_attr_perry_global, set_attr_perry_global) = IdentifierAttr::new(name);
    prepare(this) {
        <Self as OneResultInterface>::verify(this, ctx)?;
        let value = this.get_result(ctx).get_type(ctx);
    }
    result(ctx, value: TypeHandle): TypeHandle =
        Ok(TypedHandle::<SlotType>::from_handle(value, ctx)?.into());
    error = crate::locals::LocalErr::NotSlot("perry.global");
}

op_schema! {
    /// Checked load that raises a `TypeError` if the module binding is read before its initializer runs.
    pub GlobalLoadOp(slot) -> fallible;
    name = "perry.global_load";
    format = "$0 ` : ` type($0)";
    constructors { pub new(slot: Value) => infer(slot); }
    result(ctx, slot: Value): TypeHandle = Ok(
        TypedHandle::<SlotType>::from_handle(slot.get_type(ctx), ctx)?
            .deref(ctx)
            .value(),
    );
    error = crate::locals::LocalErr::Type("perry.global_load");
}
