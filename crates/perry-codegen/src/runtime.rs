//! Runtime and helper-library functions that compiled code imports, each declared once
//! with its Wasm signature. Calls name a [Runtime] variant; the import is emitted once per
//! module for every function used.
use perry_mir::Intrinsic;
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::TypeHandle,
};

/// Runtime ABI carriers; `F64Bits` preserves optional-number sentinels through an i64.
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
    (@carrier I32) => { W::I32 };
    (@carrier I64) => { W::I64 };
    (@carrier F64) => { W::F64 };
    (@carrier F64Bits) => { W::F64Bits };
    (@carrier Number) => { W::F64 };
    (@carrier Abort) => { W::I32 };
    (@carrier BigInt) => { W::I32 };
    (@carrier Bool) => { W::I32 };
    (@carrier ByobReader) => { W::I32 };
    (@carrier ByteBuffer) => { W::I32 };
    (@carrier ByteInput) => { W::I32 };
    (@carrier Bytes) => { W::I32 };
    (@carrier Decoder) => { W::I32 };
    (@carrier Exception) => { W::I32 };
    (@carrier Headers) => { W::I32 };
    (@carrier Incoming) => { W::I32 };
    (@carrier Index) => { W::I32 };
    (@carrier Json) => { W::I32 };
    (@carrier NullReadable) => { W::I32 };
    (@carrier NullString) => { W::I32 };
    (@carrier NullStringArray) => { W::I32 };
    (@carrier NumberArray) => { W::I32 };
    (@carrier OptBytes) => { W::I32 };
    (@carrier OptString) => { W::I32 };
    (@carrier PromiseBytes) => { W::I32 };
    (@carrier PromiseJson) => { W::I32 };
    (@carrier PromiseReadResult) => { W::I32 };
    (@carrier PromiseResponse) => { W::I32 };
    (@carrier PromiseStats) => { W::I32 };
    (@carrier PromiseString) => { W::I32 };
    (@carrier PromiseStringArray) => { W::I32 };
    (@carrier PromiseVoid) => { W::I32 };
    (@carrier ReadResult) => { W::I32 };
    (@carrier Readable) => { W::I32 };
    (@carrier Reader) => { W::I32 };
    (@carrier Reference) => { W::I32 };
    (@carrier Regex) => { W::I32 };
    (@carrier Request) => { W::I32 };
    (@carrier Response) => { W::I32 };
    (@carrier Scalar) => { W::I32 };
    (@carrier SearchParams) => { W::I32 };
    (@carrier Stats) => { W::I32 };
    (@carrier String) => { W::I32 };
    (@carrier StringArray) => { W::I32 };
    (@carrier Url) => { W::I32 };
    (@carrier Writable) => { W::I32 };
    (@carrier Writer) => { W::I32 };
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
