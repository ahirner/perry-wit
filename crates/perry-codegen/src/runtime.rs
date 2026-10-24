//! Runtime and standard-library functions imported by compiled code, each declared once
//! with its Wasm signature. Calls reference a [Runtime] variant, and each used function is
//! emitted once in the module's import section.
use perry_mir::Intrinsic;
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::TypeHandle,
};

/// Runtime ABI carriers, where `F64Bits` preserves optional-number sentinels in an `i64`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum W {
    I32,
    I64,
    F64,
    F64Bits,
}

impl W {
    fn mir(self, ctx: &mut Context) -> TypeHandle {
        match self {
            W::I32 => IntegerType::get(ctx, 32, Signedness::Signless).into(),
            W::I64 | W::F64Bits => IntegerType::get(ctx, 64, Signedness::Signless).into(),
            W::F64 => FP64Type::get(ctx).into(),
        }
    }
}

macro_rules! runtime_functions {
    (
        inline { $($inline:tt)* }
        runtime {
            $($variant:ident = $name:literal
                $(($($param:ident),*))?
                $([$($intrinsic:ident = $mir_name:literal ($($operand:ident),*) -> ($($output:ident)?),
                    $effect:ident, $allocation:ident $(=> $contract:ident)?;)+])?
                $(-> $result:ident)?;)*
        }
    ) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
        pub(crate) enum Runtime {
            $($variant,)*
        }

        impl Runtime {
            pub(crate) fn for_intrinsic(intrinsic: Intrinsic) -> Option<Self> {
                match intrinsic {
                    $($($(Intrinsic::$intrinsic)|+ => Some(Self::$variant),)?)*
                    _ => None,
                }
            }

            pub(crate) fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            fn params(self) -> &'static [W] {
                match self {
                    $(Self::$variant => runtime_functions!(@params
                        $(($($param),*))? $($(($($operand),*))+)?),)*
                }
            }

            pub(crate) fn result(self) -> Option<W> {
                match self {
                    $(Self::$variant => runtime_functions!(@result
                        $(($result))? $($(($($output)?))+)?),)*
                }
            }
        }
    };
    (@params ($($param:ident),*) $($alias:tt)*) => {
        &[$(runtime_functions!(@carrier $param)),*]
    };
    (@result) => { None };
    (@result () $($alias:tt)*) => { None };
    (@result ($result:ident) $($alias:tt)*) => { Some(runtime_functions!(@carrier $result)) };
    (@carrier I64) => { W::I64 };
    (@carrier F64) => { W::F64 };
    (@carrier F64Bits) => { W::F64Bits };
    (@carrier Number) => { W::F64 };
    (@carrier $other:ident) => { W::I32 };
}

perry_mir::intrinsic_catalog!(runtime_functions);

impl Runtime {
    /// The symbol of the import in the compiled module.
    pub(crate) fn symbol(self) -> Identifier {
        let name = self.name();
        format!("rt_{}", name.strip_prefix("perry_").unwrap_or(name))
            .try_into()
            .expect("runtime names are identifiers")
    }

    /// MIR parameter and result types of the import.
    pub(crate) fn signature(self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let params = self.params().iter().map(|w| w.mir(ctx)).collect();
        let results = self.result().map(|w| w.mir(ctx)).into_iter().collect();
        (params, results)
    }
}
