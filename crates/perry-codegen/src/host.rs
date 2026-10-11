//! Canonical built-ins and host functions that compiled code imports directly, outside the
//! runtime's helper module.
use crate::{MONOTONIC_CLOCK, boundary, repr::i32_type};
use pliron::{
    builtin::types::{IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::TypeHandle,
};

/// Canonical built-ins and host functions the module may import.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Host {
    ContextGet0,
    ContextSet0,
    TaskCancel,
    WaitFor,
    Foreign(String, String, Vec<boundary::Flat>, Vec<boundary::Flat>),
    /// `task.return` of an async export: (module, function, flat result types).
    TaskReturn(String, String, Vec<boundary::Flat>),
}

impl Host {
    /// The symbol of the import in the compiled module.
    pub(crate) fn symbol(&self) -> Identifier {
        match self {
            Host::Foreign(module, name, _, _) => {
                // Hex-encode `module#name`: import names may contain any character, and
                // sanitizing alone could map two imports to one symbol.
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let mut hex = String::new();
                for byte in module
                    .bytes()
                    .chain(std::iter::once(b'#'))
                    .chain(name.bytes())
                {
                    hex.push(char::from(HEX[usize::from(byte >> 4)]));
                    hex.push(char::from(HEX[usize::from(byte & 0xF)]));
                }
                symbol("host_foreign_", &hex)
            }
            Host::ContextGet0 => pliron::ident!("host_context_get_0"),
            Host::ContextSet0 => pliron::ident!("host_context_set_0"),
            Host::TaskCancel => pliron::ident!("host_task_cancel"),
            Host::WaitFor => pliron::ident!("host_wait_for"),
            Host::TaskReturn(module, function, _) => {
                symbol("host_task_return_", &format!("{module}_{function}"))
            }
        }
    }

    /// Import module and field.
    pub(crate) fn into_path(self) -> (String, String) {
        let root = |name: &str| ("$root".to_string(), name.to_string());
        match self {
            Host::Foreign(module, name, _, _) => (module, name),
            Host::ContextGet0 => root("[context-get-0]"),
            Host::ContextSet0 => root("[context-set-0]"),
            Host::TaskCancel => ("[export]$root".into(), "[task-cancel]".into()),
            Host::WaitFor => (
                MONOTONIC_CLOCK.to_string(),
                "[async-lower]wait-for".to_string(),
            ),
            Host::TaskReturn(module, function, _) => (module, format!("[task-return]{function}")),
        }
    }

    /// Wasm parameter and result types of the import.
    pub(crate) fn signature(&self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let i32 = i32_type(ctx);
        let i64: TypeHandle = IntegerType::get(ctx, 64, Signedness::Signless).into();
        match self {
            Host::Foreign(_, _, params, results) => (
                params
                    .iter()
                    .map(|f| boundary::flat_type(ctx, *f))
                    .collect(),
                results
                    .iter()
                    .map(|f| boundary::flat_type(ctx, *f))
                    .collect(),
            ),
            Host::ContextGet0 => (vec![], vec![i32]),
            Host::ContextSet0 => (vec![i32], vec![]),
            Host::TaskCancel => (vec![], vec![]),
            Host::WaitFor => (vec![i64], vec![i32]),
            Host::TaskReturn(_, _, flat) => (
                flat.iter().map(|f| boundary::flat_type(ctx, *f)).collect(),
                vec![],
            ),
        }
    }
}

/// The symbol `prefix` followed by `name`, with every character of `name` outside ASCII
/// alphanumerics replaced by `_`. Distinct names may share a symbol.
pub(crate) fn symbol(prefix: &'static str, name: &str) -> Identifier {
    let mut text = String::with_capacity(prefix.len() + name.len());
    text.push_str(prefix);
    text.extend(
        name.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }),
    );
    Identifier::try_new(text).expect(
        "an identifier prefix followed by ASCII alphanumerics and underscores is an identifier",
    )
}
