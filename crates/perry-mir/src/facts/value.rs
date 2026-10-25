use super::{Fact, Integer, Outcomes, Range};

/// Requirements join by width and observation strength. Retaining the source carrier is
/// terminal: later integer consumers convert at their uses instead of retyping the value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Demand {
    #[default]
    None,
    Truncated(Option<Integer>),
    Observed(Option<Integer>),
    Original,
}

impl Demand {
    pub(super) fn width(self) -> Option<Integer> {
        match self {
            Self::Truncated(integer) | Self::Observed(integer) => integer,
            Self::None | Self::Original => None,
        }
    }

    fn join(self, other: Self) -> Self {
        let width = match (self.width(), other.width()) {
            (Some(a), Some(b)) => Some(a.common(b)),
            (a, b) => a.or(b),
        };
        match (self, other) {
            (Self::Original, _) | (_, Self::Original) => Self::Original,
            (Self::Observed(_), _) | (_, Self::Observed(_)) => Self::Observed(width),
            (Self::Truncated(_), _) | (_, Self::Truncated(_)) => Self::Truncated(width),
            (Self::None, Self::None) => Self::None,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct ValueState {
    facts: ValueFacts,
    demand: Demand,
}

impl ValueState {
    pub(super) fn new(facts: ValueFacts) -> Self {
        Self {
            facts,
            demand: Demand::None,
        }
    }

    pub(super) fn facts(&self) -> ValueFacts {
        self.facts
    }

    pub(super) fn demand(&self) -> Demand {
        self.demand
    }

    pub(super) fn require(&mut self, incoming: Demand) -> bool {
        let demand = self.demand.join(incoming);
        if demand == self.demand {
            return false;
        }
        self.demand = demand;
        if demand == Demand::Original {
            self.facts.domain = Fact::Original;
        }
        true
    }

    pub(super) fn update(&mut self, incoming: ValueFacts, specialize: bool) -> (bool, bool) {
        let representation = Representation::value(
            specialize && self.demand != Demand::Original,
            self.demand.width().map(Integer::register),
        );
        self.facts.join(incoming, representation)
    }
}

/// Numeric representation, interval and promise settlement travel through the same edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ValueFacts {
    pub(super) domain: Fact,
    pub(super) range: Range,
    pub(super) outcome: Outcomes,
    expanded: bool,
    /// An integer literal outside the demanded width reached this value on an exact (not
    /// widening) join, so narrowing it would change the value rather than wrap an overflow.
    pub(super) inexact: bool,
}

#[derive(Clone, Copy)]
pub(super) enum Representation {
    Original(Option<Integer>),
    Specialized(Option<Integer>),
    Storage(Option<Integer>),
}

impl Representation {
    pub(super) fn value(specialize: bool, required: Option<Integer>) -> Self {
        if specialize {
            Self::Specialized(required)
        } else {
            Self::Original(required)
        }
    }

    fn resolve(self, domain: Fact) -> (Fact, Option<Integer>) {
        let domain = if matches!(self, Self::Original(_)) && domain == Fact::Numeric {
            Fact::Original
        } else {
            domain
        };
        let bounds = match self {
            Self::Storage(declared) => declared.or(domain.integer()),
            Self::Original(required) | Self::Specialized(required) => match domain {
                Fact::Literal { .. } => required,
                _ => domain.integer(),
            },
        };
        (domain, bounds)
    }
}

impl ValueFacts {
    pub(super) fn numeric(domain: Fact, range: Range) -> Self {
        Self {
            domain,
            range,
            ..Self::default()
        }
    }

    /// Returns whether facts changed and whether an interval widened. Storage joins widen
    /// immediately, while SSA values and return summaries allow one exact expansion first.
    pub(super) fn join(&mut self, other: Self, representation: Representation) -> (bool, bool) {
        let (domain, bounds) = representation.resolve(self.domain.join(other.domain));
        let step = self.range.join_step(
            other.range,
            bounds,
            self.expanded || matches!(representation, Representation::Storage(_)),
        );
        let outcome = self.outcome.join(other.outcome);
        let inexact = self.inexact
            || other.inexact
            || (step.clamped && matches!(domain, Fact::Literal { .. }));
        let changed = (domain, step.range, outcome, inexact)
            != (self.domain, self.range, self.outcome, self.inexact);
        self.domain = domain;
        self.range = step.range;
        self.outcome = outcome;
        self.expanded |= step.expanded;
        self.inexact = inexact;
        (changed, step.widened)
    }
}
