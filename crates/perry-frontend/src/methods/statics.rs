//! Static built-ins selected on the upstream global receiver.
use crate::engine::types::{NameId, TypeTable};
use crate::hir_compat::{MemberKey, member};
use crate::methods::Kind;
use perry_hir::ir::Expr;
use perry_mir::Combinator;

#[derive(Clone, Copy, Debug)]
pub(crate) enum StaticMethod {
    PerformanceNow,
    Resolve,
    Reject,
    Combine(Combinator),
    Temporal(TemporalFactory),
    Unsupported(NameId),
}

macro_rules! temporal_factories {
    ($( $class:literal { $( $factory:ident: $name:literal -> $kind:ident ),+ } )+) => {
        #[derive(Clone, Copy, Debug)]
        pub(crate) enum TemporalFactory { $($($factory,)+)+ }

        impl TemporalFactory {
            fn select(class: &str, method: &str) -> Option<Self> {
                match (class, method) { $($(($class, $name) => Some(Self::$factory),)+)+ _ => None }
            }

            pub(crate) fn result(self) -> Kind {
                match self { $($(Self::$factory => Kind::$kind,)+)+ }
            }

            pub(crate) fn name(self) -> &'static str {
                match self { $($(Self::$factory => $name,)+)+ }
            }
        }
    };
}

temporal_factories! {
    "Now" { NowInstant: "instant" -> Instant, NowDate: "plainDateISO" -> PlainDate,
            NowDateTime: "plainDateTimeISO" -> PlainDateTime, NowZone: "timeZoneId" -> String }
    "Instant" { InstantFrom: "from" -> Instant, InstantFromMs: "fromEpochMilliseconds" -> Instant }
    "PlainDate" { DateFrom: "from" -> PlainDate, CompareDates: "compare" -> Number }
    "PlainDateTime" { DateTimeFrom: "from" -> PlainDateTime }
}

impl StaticMethod {
    pub(crate) fn select(expr: &Expr, types: &mut TypeTable) -> Option<Self> {
        let Expr::Call { callee, .. } = expr else {
            return None;
        };
        let (object, MemberKey::Named(method, _)) = member(callee)? else {
            return None;
        };
        let (parent, MemberKey::Named(class, _)) = member(object)? else {
            return None;
        };
        let method = crate::hir_compat::method(method).name();
        if matches!(parent, Expr::GlobalGet(_)) {
            return Some(match (class, method) {
                ("performance", "now") => Self::PerformanceNow,
                ("Promise", "resolve") => Self::Resolve,
                ("Promise", "reject") => Self::Reject,
                ("Promise", "all") => Self::Combine(Combinator::All),
                ("Promise", "allSettled") => Self::Combine(Combinator::AllSettled),
                ("Promise", "race") => Self::Combine(Combinator::Race),
                ("performance" | "Promise", _) => Self::Unsupported(
                    types.intern_name(&format!("{class}.{method} is not supported yet")),
                ),
                _ => return None,
            });
        }
        let (Expr::GlobalGet(_), MemberKey::Named("Temporal", _)) = member(parent)? else {
            return None;
        };
        if let Some(factory) = TemporalFactory::select(class, method) {
            return Some(Self::Temporal(factory));
        }
        let message = format!(
            "Temporal.{class}.{method} is not supported; use Instant.from, Instant.fromEpochMilliseconds, PlainDate.from, PlainDate.compare, PlainDateTime.from or Temporal.Now"
        );
        Some(Self::Unsupported(types.intern_name(&message)))
    }
}
