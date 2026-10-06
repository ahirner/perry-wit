//! Pure lifecycle model and a source emitter for sequential retained-task traces.
use crate::{
    execution::{Evidence, Outcome, check_equivalence, number},
    program::INPUTS,
    registry::Case,
};
use anyhow::{Result, ensure};
use proptest::prelude::*;
use proptest_state_machine::ReferenceStateMachine;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default)]
pub enum TimerModel {
    #[default]
    Idle,
    Pending,
    Settled {
        cancelled: bool,
        observed: bool,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Start,
    Release,
    Cancel,
    Observe,
    Reuse,
}

impl ReferenceStateMachine for TimerModel {
    type State = Self;
    type Transition = Action;
    fn init_state() -> BoxedStrategy<Self> {
        Just(Self::default()).boxed()
    }
    fn transitions(state: &Self) -> BoxedStrategy<Action> {
        let actions = [
            Action::Start,
            Action::Release,
            Action::Cancel,
            Action::Observe,
            Action::Reuse,
        ]
        .into_iter()
        .filter(|action| Self::preconditions(state, action))
        .collect::<Vec<_>>();
        prop::sample::select(actions).boxed()
    }
    fn preconditions(state: &Self, action: &Action) -> bool {
        matches!(
            (state, action),
            (Self::Idle, Action::Start)
                | (Self::Pending, Action::Release | Action::Cancel)
                | (Self::Settled { .. }, Action::Observe)
                | (Self::Settled { observed: true, .. }, Action::Reuse)
        )
    }
    fn apply(state: Self, action: &Action) -> Self {
        assert!(Self::preconditions(&state, action));
        match action {
            Action::Start => Self::Pending,
            Action::Release => Self::Settled {
                cancelled: false,
                observed: false,
            },
            Action::Cancel => Self::Settled {
                cancelled: true,
                observed: false,
            },
            Action::Observe => match state {
                Self::Settled { cancelled, .. } => Self::Settled {
                    cancelled,
                    observed: true,
                },
                _ => unreachable!(),
            },
            Action::Reuse => Self::Idle,
        }
    }
}

pub fn expected_trace(actions: &[Action]) -> Vec<String> {
    let mut state = TimerModel::Idle;
    actions
        .iter()
        .map(|action| {
            let value = match action {
                Action::Start => 1.0,
                Action::Release => 2.0,
                Action::Cancel => 3.0,
                Action::Reuse => 5.0,
                Action::Observe => {
                    if matches!(
                        state,
                        TimerModel::Settled {
                            cancelled: true,
                            ..
                        }
                    ) {
                        4.0
                    } else {
                        7.0
                    }
                }
            };
            state = TimerModel::apply(state.clone(), action);
            number(value)
        })
        .collect()
}

pub fn check(case: &Case, evidence: &Evidence) -> Result<()> {
    check_equivalence(case, evidence)?;
    let Case::Lifecycle(actions) = case else {
        anyhow::bail!("lifecycle checker requires an action trace")
    };
    let expected = expected_trace(actions);
    for outcome in [&evidence.oracle, &evidence.component] {
        let Outcome::Values { observations, .. } = outcome else {
            anyhow::bail!("missing lifecycle observations")
        };
        for (observation, input) in observations.iter().zip(INPUTS) {
            ensure!(
                observation.value == number(input) && observation.trace == expected,
                "lifecycle model mismatch: {observation:?}"
            );
        }
    }
    Ok(())
}

pub fn source(actions: &[Action]) -> String {
    let mut source = String::from(
        "import {setTimeout} from 'node:timers/promises';\nexport async function run(x:number):Promise<{value:number,trace:number[]}>{const trace:number[]=[];\n",
    );
    let mut generation = 0;
    let mut state = TimerModel::Idle;
    let mut observation = 0;
    for action in actions {
        match action {
            Action::Start => source.push_str(&format!("const controller{generation}=new AbortController();const task{generation}=setTimeout(0,7,{{signal:controller{generation}.signal}});trace.push(1);\n")),
            Action::Release => source.push_str(&format!("await task{generation};trace.push(2);\n")),
            Action::Cancel => source.push_str(&format!("controller{generation}.abort();trace.push(3);\n")),
            Action::Observe => if matches!(state,TimerModel::Settled{cancelled:true,..}) {
                source.push_str(&format!("let rejected{observation}=false;try{{await task{generation};}}catch{{rejected{observation}=true;}}if(!rejected{observation})throw 90;trace.push(4);\n"));
                observation+=1;
            } else { source.push_str(&format!("trace.push(await task{generation});\n")); },
            Action::Reuse => { source.push_str("trace.push(5);\n"); generation += 1; }
        }
        state = TimerModel::apply(state, action);
    }
    if !matches!(state, TimerModel::Idle) {
        source.push_str(&format!("try{{await task{generation};}}catch{{}}\n"));
    }
    source.push_str("return {value:x,trace:trace};}");
    source
}
