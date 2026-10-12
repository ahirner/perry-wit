//! Canonical built-ins and host functions that compiled code imports directly, outside the
//! runtime's helper module.
use crate::{
    MONOTONIC_CLOCK,
    boundary::{self, Flat},
    repr::i32_type,
};
use pliron::{
    builtin::types::{IntegerType, Signedness},
    context::Context,
    ident,
    identifier::{Identifier, Legaliser},
    r#type::TypeHandle,
};

/// An import of the module. Whether its call yields a value is part of its type, so a call
/// site cannot read a result the import does not declare.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Host {
    Value(ValueHost),
    Effect(EffectHost),
}

/// Imports called for exactly one result.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ValueHost {
    /// `context.get 0`: the task an async export stored.
    ContextGet0,
    /// `wait-for` of the monotonic clock, lowered asynchronously: a subtask status.
    WaitFor,
    /// A component import with one flat result.
    Foreign(Foreign, Flat),
}

/// Imports called for their effect only.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EffectHost {
    ContextSet0,
    TaskCancel,
    /// A component import without flat results.
    Foreign(Foreign),
    /// `task.return` of an async export: (module, function, flat result types).
    TaskReturn(String, String, Vec<Flat>),
}

/// A component import: its module, field and flat parameter types.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Foreign {
    pub(crate) module: String,
    pub(crate) name: String,
    pub(crate) params: Vec<Flat>,
}

impl Host {
    /// The symbol of the import in the compiled module.
    pub(crate) fn symbol(&self) -> Identifier {
        match self {
            Host::Value(ValueHost::Foreign(foreign, _))
            | Host::Effect(EffectHost::Foreign(foreign)) => foreign.symbol(),
            Host::Value(ValueHost::ContextGet0) => ident!("host_context_get_0"),
            Host::Value(ValueHost::WaitFor) => ident!("host_wait_for"),
            Host::Effect(EffectHost::ContextSet0) => ident!("host_context_set_0"),
            Host::Effect(EffectHost::TaskCancel) => ident!("host_task_cancel"),
            Host::Effect(EffectHost::TaskReturn(module, function, _)) => {
                symbol(ident!("host_task_return_"), &format!("{module}_{function}"))
            }
        }
    }

    /// Import module and field.
    pub(crate) fn into_path(self) -> (String, String) {
        let root = |name: &str| ("$root".to_string(), name.to_string());
        match self {
            Host::Value(ValueHost::Foreign(foreign, _))
            | Host::Effect(EffectHost::Foreign(foreign)) => (foreign.module, foreign.name),
            Host::Value(ValueHost::ContextGet0) => root("[context-get-0]"),
            Host::Value(ValueHost::WaitFor) => (
                MONOTONIC_CLOCK.to_string(),
                "[async-lower]wait-for".to_string(),
            ),
            Host::Effect(EffectHost::ContextSet0) => root("[context-set-0]"),
            Host::Effect(EffectHost::TaskCancel) => {
                ("[export]$root".into(), "[task-cancel]".into())
            }
            Host::Effect(EffectHost::TaskReturn(module, function, _)) => {
                (module, format!("[task-return]{function}"))
            }
        }
    }

    /// Wasm parameter and result types of the import: one result exactly for [Host::Value].
    pub(crate) fn signature(&self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let flat = |ctx: &mut Context, flat: &[Flat]| -> Vec<TypeHandle> {
            flat.iter().map(|f| boundary::flat_type(ctx, *f)).collect()
        };
        match self {
            Host::Value(host) => {
                let (params, result) = match host {
                    ValueHost::ContextGet0 => (vec![], i32_type(ctx)),
                    ValueHost::WaitFor => (
                        vec![IntegerType::get(ctx, 64, Signedness::Signless).into()],
                        i32_type(ctx),
                    ),
                    ValueHost::Foreign(foreign, result) => (
                        flat(ctx, &foreign.params),
                        boundary::flat_type(ctx, *result),
                    ),
                };
                (params, vec![result])
            }
            Host::Effect(host) => {
                let params = match host {
                    EffectHost::ContextSet0 => vec![i32_type(ctx)],
                    EffectHost::TaskCancel => vec![],
                    EffectHost::Foreign(foreign) => flat(ctx, &foreign.params),
                    EffectHost::TaskReturn(_, _, results) => flat(ctx, results),
                };
                (params, vec![])
            }
        }
    }
}

impl Foreign {
    /// Hex-encoded `module#name`: import names may contain any character, and sanitizing
    /// alone could map two imports to one symbol.
    fn symbol(&self) -> Identifier {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(2 * (self.module.len() + 1 + self.name.len()));
        for byte in self
            .module
            .bytes()
            .chain(std::iter::once(b'#'))
            .chain(self.name.bytes())
        {
            hex.push(char::from(HEX[usize::from(byte >> 4)]));
            hex.push(char::from(HEX[usize::from(byte & 0xF)]));
        }
        symbol(ident!("host_foreign_"), &hex)
    }
}

/// The symbol `prefix` followed by `name`, with every character of `name` outside ASCII
/// alphanumerics replaced by `_`. Distinct names may share a symbol.
///
/// Legalising after a valid identifier only replaces characters, so every name has a symbol.
pub(crate) fn symbol(prefix: Identifier, name: &str) -> Identifier {
    Identifier::try_new(format!("{prefix}{name}"))
        .unwrap_or_else(|_| Legaliser::default().legalise(&format!("{prefix}{name}")))
}
