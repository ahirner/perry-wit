//! WIT-free public signatures checked against Node execution.
use crate::{
    execution::check_equivalence,
    registry::{Case, Contract, Witness},
};
use proptest::prelude::*;

fn case(value: i32, asynchronous: bool) -> Case {
    let prefix = if asynchronous { "async " } else { "" };
    let result = if asynchronous {
        "Promise<{value:number,trace:number[]}>"
    } else {
        "{value:number,trace:number[]}"
    };
    Case::Inferred {
        asynchronous,
        source: format!(
            r#"
        type Data = {{ absent?: number; nullable: number | null; both: string | null | undefined }};
        function inspect(data:Data):number {{
            if(data.absent !== undefined) return 1;
            if(data.nullable !== null) return 2;
            if(data.both !== undefined) return 3;
            return 4;
        }}
        export {prefix}function run(x:number):{result} {{
            const data:Data = {{nullable:null,both:undefined}};
            const wide = 9007199254740993n + BigInt({value});
            return {{value:x,trace:[inspect(data),Number(wide-9007199254740993n)]}};
        }}
    "#
        ),
    }
}

pub(crate) fn contract() -> Contract {
    Contract::new(
        "wit.inferred.signatures",
        vec![
            Witness {
                partition: "synchronous-aggregates",
                case: case(-1, false),
            },
            Witness {
                partition: "asynchronous-aggregates",
                case: case(1, true),
            },
        ],
        (-1000i32..1000, any::<bool>())
            .prop_map(|(value, asynchronous)| case(value, asynchronous))
            .boxed(),
        check_equivalence,
    )
}
