//! Operation declarations, constructors and verification from one schema.
macro_rules! op_schema {
    (
        $(#[$meta:meta])* $vis:vis $op:ident($($operands:tt)*) -> $mode:ident;
        name = $name:literal;
        format = $format:literal;
        $(interfaces = [$($interface:path),* $(,)?];)?
        $(attributes = ($($attributes:tt)*);)?
        constructors { $($constructors:tt)* }
        $(attribute($attr:ident $(: $attr_ty:ty)?, $get:ident, $set:ident) = $value:expr $(=> $missing:expr)?;)*
        $(prepare($this:ident) { $($prepare:tt)* })?
        result($ctx:ident $(, $input:ident : $input_ty:ty)*) : $ty:ty = $result:expr;
        error = $error:expr;
        $(invalid_source = $source_error:expr;)?
        $(require $required:expr;)*
        $(check_before $precondition:expr => $pre_error:expr;)*
        $(check $condition:expr => $diagnostic:expr;)*
    ) => {
        op_schema!(@definition
            [$(#[$meta])* $vis struct $op;]
            [name = $name, format = $format, $(attributes = ($($attributes)*),)?]
            [$($($interface,)*)?] ([$($operands)*] [$($source_error)?]); $($constructors)*
        );
        impl $op {
            $(op_schema!(@accessor $attr, $get $(: $attr_ty)?);)*
            fn result_contract($ctx: &Context, $($input: $input_ty),*) -> op_schema!(@type $mode, $ty) {
                let _ = $ctx;
                $result
            }
            op_schema!(@construct $ctx, $mode, [$($operands)*],
                [$(attribute($set) = $value;)*]; $($constructors)*
            );
        }
        impl Verify for $op {
            fn verify(&self, $ctx: &Context) -> Result<()> {
                op_schema!(@bind self, $ctx, [$($operands)*] $(, $source_error)?);
                $(op_schema!(@check_attr self, $ctx, $attr, $get $(=> $missing)?);)*
                $(let $this = self; $($prepare)*)?
                $(let _ = $required;)*
                $(if !$precondition {
                    pliron::verify_err!(self.loc($ctx), $pre_error)?;
                })*
                let expected = op_schema!(@verify self, $ctx, $mode,
                    Self::result_contract($ctx, $($input),*), $error $(, $source_error)?
                );
                if !$crate::signatures::OutputContract::accepts(
                    expected, $ctx, &self.get_operation().deref($ctx)
                ) {
                    pliron::verify_err!(self.loc($ctx), $error)?;
                }
                $(if !$condition {
                    pliron::verify_err!(self.loc($ctx), $diagnostic)?;
                })*
                Ok(())
            }
        }
    };
    (@accessor $attr:ident, $get:ident : $attr_ty:ty) => {
        pub fn $attr(&self, ctx: &Context) -> $attr_ty {
            self.$get(ctx).expect(stringify!($attr)).clone().into()
        }
    };
    (@accessor $attr:ident, $get:ident) => {};
    (@check_attr $op:ident, $ctx:ident, $attr:ident, $get:ident => $missing:expr) => {
        #[allow(unused_variables)]
        let Some($attr) = $op.$get($ctx) else {
            return pliron::verify_err!($op.loc($ctx), $missing);
        };
    };
    (@check_attr $op:ident, $ctx:ident, $attr:ident, $get:ident) => {};
    // Constructor result policies determine cardinality; operand names determine arity.
    (@definition $item:tt $metadata:tt $interfaces:tt $operands:tt;
        $(#[$meta:meta])* $vis:vis $name:ident($($param:tt)*) $(operands($values:expr))?
        $(-> $mode:ident)? => $policy:ident($($value:tt)*); $($rest:tt)*
    ) => {
        op_schema!(@outputs $policy, $item $metadata $interfaces $operands);
    };
    (@outputs empty, $item:tt $metadata:tt [$($interfaces:tt)*] $operands:tt) => {
        op_schema!(@inputs $operands $item $metadata
            [pliron::builtin::op_interfaces::NResultsInterface<0>, $($interfaces)*]);
    };
    (@outputs results, $item:tt $metadata:tt $interfaces:tt $operands:tt) => {
        op_schema!(@inputs $operands $item $metadata $interfaces);
    };
    (@outputs $single:ident, $item:tt $metadata:tt [$($interfaces:tt)*] $operands:tt) => {
        op_schema!(@inputs $operands $item $metadata
            [pliron::builtin::op_interfaces::OneResultInterface, $($interfaces)*]);
    };
    // A custom source diagnostic owns arity validation instead of a generic interface.
    (@inputs ([$($operands:tt)*] [$error:expr]) $item:tt $metadata:tt $interfaces:tt) => {
        op_schema!(@emit $item $metadata $interfaces);
    };
    (@inputs ([$($operands:tt)*] []) $item:tt $metadata:tt $interfaces:tt) => {
        op_schema!(@inputs [$($operands)*] $item $metadata $interfaces);
    };
    (@inputs [..$values:ident] $item:tt $metadata:tt $interfaces:tt) => {
        op_schema!(@emit $item $metadata $interfaces);
    };
    (@inputs [$arg:ident] $item:tt $metadata:tt [$($interfaces:tt)*]) => {
        op_schema!(@emit $item $metadata
            [pliron::builtin::op_interfaces::OneOpdInterface, $($interfaces)*]);
    };
    (@inputs [$($arg:ident),*] $item:tt $metadata:tt [$($interfaces:tt)*]) => {
        op_schema!(@emit $item $metadata
            [pliron::builtin::op_interfaces::NOpdsInterface<{op_schema!(@count $($arg),*)}>, $($interfaces)*]);
    };
    (@emit [$(#[$meta:meta])* $vis:vis struct $op:ident;] [$($metadata:tt)*] [$($interfaces:tt)*]) => {
        $(#[$meta])*
        #[allow(rustdoc::private_intra_doc_links, reason = "pliron documents its generated hidden attribute keys")]
        #[pliron::derive::pliron_op($($metadata)* interfaces = [$($interfaces)*])]
        $vis struct $op;
    };
    (@count $($arg:ident),*) => { <[&str]>::len(&[$(stringify!($arg)),*]) };
    (@construct $ctx:ident, $mode:ident, [$($operands:tt)*], [$($init:tt)*];
        $(#[$meta:meta])* $vis:vis $name:ident($($param:ident : $param_ty:ty),*) $(operands($values:expr))? $(-> $constructor_mode:ident)? => $policy:ident($($value:expr),*); $($rest:tt)*
    ) => {
        $(#[$meta])*
        $vis fn $name($ctx: &mut Context, $($param: $param_ty),*) -> op_schema!(@type $mode $(, $constructor_mode)?, Self) {
            let types = op_schema!(@results $ctx, $mode, $policy($($value),*));
            let op = Operation::new_op::<Self>(
                $ctx, types, op_schema!(@operands [$($operands)*] $(, $values)?), vec![], 0
            );
            op_schema!(@initialize $ctx, op; $($init)*);
            op_schema!(@wrap $mode $(, $constructor_mode)?, op)
        }
        op_schema!(@construct $ctx, $mode, [$($operands)*], [$($init)*]; $($rest)*);
    };
    (@construct $ctx:ident, $mode:ident, [$($operands:tt)*], [$($init:tt)*];) => {};
    (@operands [$($operands:tt)*], $values:expr) => { $values };
    (@operands [$($arg:ident),*]) => { vec![$($arg),*] };
    (@bind $op:ident, $ctx:ident, [..$values:ident]) => {
        let operation = $op.get_operation().deref($ctx);
        let $values = &*operation;
    };
    (@bind $op:ident, $ctx:ident, [$($arg:ident),*] $(, $error:expr)?) => {
        op_schema!(@arity $op, $ctx, [$($arg),*] $(, $error)?);
        op_schema!(@values $op, $ctx, 0; $($arg),*);
    };
    (@values $op:ident, $ctx:ident, $index:expr; $arg:ident $(, $rest:ident)*) => {
        let $arg = $op.get_operation().deref($ctx).get_operand($index);
        op_schema!(@values $op, $ctx, $index + 1; $($rest),*);
    };
    (@values $op:ident, $ctx:ident, $index:expr;) => {};
    (@initialize $ctx:ident, $op:ident; attribute($set:ident) = $value:expr; $($rest:tt)*) => {
        $op.$set($ctx, $value);
        op_schema!(@initialize $ctx, $op; $($rest)*);
    };
    (@initialize $ctx:ident, $op:ident;) => {};
    (@results $ctx:ident, $mode:ident, infer($($input:expr),*)) => {
        vec![$crate::signatures::ResultType::get(
            op_schema!(@infer $mode, Self::result_contract($ctx, $($input),*)), $ctx
        )]
    };
    (@results $ctx:ident, $mode:ident, results($types:expr)) => { $types };
    (@results $ctx:ident, $mode:ident, explicit($ty:expr)) => { vec![$ty] };
    (@results $ctx:ident, $mode:ident, empty()) => { vec![] };
    (@arity $op:ident, $ctx:ident, [$($arg:ident),*] $(, $error:expr)?) => {
        let expected = op_schema!(@count $($arg),*);
        let actual = $op.get_operation().deref($ctx).get_num_operands();
        if actual != expected {
            pliron::verify_err!(
                $op.loc($ctx),
                op_schema!(@arity_error expected, actual $(, $error)?)
            )?;
        }
    };
    (@arity_error $expected:ident, $actual:ident, $error:expr) => { $error };
    (@arity_error $expected:ident, $actual:ident) => {
        pliron::builtin::op_interfaces::NOpdsVerifyErr($expected, $actual)
    };
    (@verify $op:ident, $ctx:ident, fallible, $value:expr, $error:expr, $source_error:expr) => {
        match $value {
            Ok(value) => value,
            Err(_) => pliron::verify_err!($op.loc($ctx), $source_error)?,
        }
    };
    (@verify $op:ident, $ctx:ident, optional, $value:expr, $error:expr) => {
        match $value {
            Some(value) => value,
            None => pliron::verify_err!($op.loc($ctx), $error)?,
        }
    };
    (@verify $op:ident, $ctx:ident, $mode:ident, $value:expr, $error:expr) => {
        op_schema!(@infer $mode, $value)
    };
    (@type $default:ident, $mode:ident, $ty:ty) => { op_schema!(@type $mode, $ty) };
    (@wrap $default:ident, $mode:ident, $value:expr) => { op_schema!(@wrap $mode, $value) };
    (@type fallible, $ty:ty) => { Result<$ty> };
    (@type optional, $ty:ty) => { Option<$ty> };
    (@type infallible, $ty:ty) => { $ty };
    (@infer fallible, $value:expr) => { $value? };
    (@infer optional, $value:expr) => { $value? };
    (@infer infallible, $value:expr) => { $value };
    (@wrap fallible, $value:expr) => { Ok($value) };
    (@wrap optional, $value:expr) => { Some($value) };
    (@wrap infallible, $value:expr) => { $value };
}

pub(super) use op_schema;
