use crate::{
    emit::Emitter,
    execution::check_equivalence,
    registry::{Case, Contract, ExitStatus, Witness},
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

const REQUIRES_ELEMENT_TYPE: &str = "an empty array literal needs a declared element type";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Element {
    Number,
    Text,
    Boolean,
    Record,
    Array,
}

impl Element {
    fn ty(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Text => "string",
            Self::Boolean => "boolean",
            Self::Record => "{id:number,name:string,tags:string[],nested:{x:number,y:number}}",
            Self::Array => "number[]",
        }
    }

    fn value(self, n: &str) -> String {
        let marked = format!("mark(trace,{n})");
        match self {
            Self::Number => marked,
            Self::Text => format!("'é_' + {marked}"),
            Self::Boolean => format!("({marked} > 0)"),
            Self::Record => format!(
                "{{id:{marked},name:'item_'+({n}),tags:['a_'+({n}),'b_'+({n})],nested:{{x:{n},y:-({n})}}}}"
            ),
            Self::Array => format!("[{marked},({n})+1]"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Form {
    Annotated,
    Aliased,
    Branches,
    Loop,
    Temporaries,
    Helper,
    AppendIndex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Write {
    value: i16,
    index: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrayCase {
    element: Element,
    seed: i16,
    writes: Vec<Write>,
    aliases: u8,
    form: Form,
}

#[derive(Clone, Copy, Debug)]
enum Conflict {
    Push,
    Index,
    Conditional,
}

impl ArrayCase {
    pub(crate) fn variants(&self) -> impl Iterator<Item = Self> + '_ {
        [
            Form::Aliased,
            Form::Branches,
            Form::Loop,
            Form::Temporaries,
            Form::Helper,
            Form::AppendIndex,
        ]
        .into_iter()
        .filter(|_| self.form == Form::Annotated)
        .map(|form| Self {
            form,
            ..self.clone()
        })
    }

    pub(crate) fn source(&self) -> String {
        self.emit(self.form, None)
    }

    fn emit(&self, form: Form, conflict: Option<Conflict>) -> String {
        let mut e = Emitter::default();
        e.line(format_args!(
            "function mark(trace:number[],n:number):number {{trace.push(n);return n;}}"
        ));
        e.line(format_args!(
            "function observe(trace:number[],text:string|undefined):void {{const bytes=new TextEncoder().encode(text);trace.push(-999,bytes.length);for(let i=0;i<bytes.length;i++){{const byte=bytes[i];if(byte===undefined)throw 91;trace.push(byte);}}}}"
        ));
        e.block(
            format_args!("export function run(x:number):{{value:number,trace:number[]}}"),
            |e| {
                e.line(format_args!("const trace:number[]=[];"));
                let ty = if form == Form::Annotated {
                    self.element.ty()
                } else {
                    "any"
                };
                e.line(format_args!("const values:({ty})[]=[];"));
                for i in 0..self.aliases {
                    let source = if i == 0 {
                        "values".into()
                    } else {
                        format!("alias{}", i - 1)
                    };
                    e.line(format_args!("const alias{i}={source};"));
                }
                let mut len = 0;
                let seed = Write {
                    value: self.seed,
                    index: None,
                };
                let mut statements = Vec::new();
                for (step, write) in std::iter::once(&seed).chain(&self.writes).enumerate() {
                    let index = write
                        .index
                        .filter(|_| len > 0)
                        .map(|index| usize::from(index) % len);
                    let value = if form == Form::Helper {
                        format!("make(trace,{})", write.value)
                    } else {
                        self.element.value(&write.value.to_string())
                    };
                    let receiver = if form == Form::Annotated
                        || step % (usize::from(self.aliases) + 1) == 0
                    {
                        "values".into()
                    } else {
                        format!("alias{}", (step - 1) % usize::from(self.aliases))
                    };
                    let assignment = |receiver: &str, value: &str| match index {
                        Some(index) => format!("{receiver}[{index}]={value};"),
                        None if form == Form::AppendIndex => format!("{receiver}[{receiver}.length]={value};"),
                        None => format!("{receiver}.push({value});"),
                    };
                    let statement = match form {
                        Form::Temporaries => format!(
                            "const item{step}={value};{}",
                            assignment(&receiver, &format!("item{step}"))
                        ),
                        Form::Branches => format!(
                            "if(x<0){{{}}}else{{{}}}",
                            assignment("values", &value),
                            assignment(&receiver, &value)
                        ),
                        _ => assignment(&receiver, &value),
                    };
                    statements.push(statement);
                    if index.is_none() {
                        len += 1;
                    }
                }
                if let Some(conflict) = conflict {
                    let receiver = if self.aliases == 0 {
                        "values".into()
                    } else {
                        format!("alias{}", self.aliases - 1)
                    };
                    let bad = "{incompatible:true}";
                    let value = match conflict {
                        Conflict::Push | Conflict::Index => bad.into(),
                        Conflict::Conditional => format!(
                            "x<0?{}:{bad}",
                            self.element.value(&self.seed.to_string())
                        ),
                    };
                    let (prefix, value) = if form == Form::Temporaries {
                        (format!("const incompatible={value};"), "incompatible".into())
                    } else {
                        (String::new(), value)
                    };
                    let statement = match conflict {
                        Conflict::Push | Conflict::Conditional => format!("{prefix}{receiver}.push({value});"),
                        Conflict::Index => format!("{prefix}{receiver}[0]={value};"),
                    };
                    statements.push(if form == Form::Branches {
                        format!("if(x<0){{{statement}}}")
                    } else {
                        statement
                    });
                }
                if form == Form::Loop {
                    e.block(
                        format_args!("for(let step=0;step<{};step++)", statements.len()),
                        |e| {
                            e.block(format_args!("switch(step)"), |e| {
                                for (i, statement) in statements.iter().enumerate() {
                                    e.line(format_args!("case {i}: {statement} break;"));
                                }
                            });
                        },
                    );
                } else {
                    for statement in statements {
                        e.line(format_args!("{statement}"));
                    }
                }
                let alias = if self.aliases == 0 {
                    "values".into()
                } else {
                    format!("alias{}", self.aliases - 1)
                };
                e.line(format_args!(
                    "observe(trace,JSON.stringify(values));observe(trace,JSON.stringify({alias}));trace.push(values.length);"
                ));
                e.line(format_args!("return {{value:x,trace}};"));
            },
        );
        if form == Form::Helper {
            e.line(format_args!("function make(trace:number[],n:number) {{const item=forward(trace,n);return item;}}"));
            e.line(format_args!(
                "function produce(trace:number[],n:number) {{return {};}}",
                self.element.value("n")
            ));
            e.line(format_args!(
                "function forward(trace:number[],n:number) {{return produce(trace,n);}}"
            ));
        }
        e.finish()
    }

    fn rejection(&self, conflict: Conflict) -> Case {
        Case::Reject {
            source: self.emit(Form::Aliased, Some(conflict)),
            variants: [Form::Branches, Form::Loop, Form::Temporaries]
                .into_iter()
                .map(|form| self.emit(form, Some(conflict)))
                .collect(),
            diagnostic: REQUIRES_ELEMENT_TYPE.into(),
        }
    }
}

fn strategy() -> BoxedStrategy<Case> {
    (
        prop_oneof![
            Just(Element::Number),
            Just(Element::Text),
            Just(Element::Boolean),
            Just(Element::Record),
            Just(Element::Array)
        ],
        -20i16..=20,
        prop::collection::vec((-20i16..=20, prop::option::of(any::<u8>())), 0..=6),
        1..=4u8,
        prop::option::of(prop_oneof![
            Just(Conflict::Push),
            Just(Conflict::Index),
            Just(Conflict::Conditional)
        ]),
    )
        .prop_map(|(element, seed, writes, aliases, conflict)| {
            let case = ArrayCase {
                element,
                seed,
                aliases,
                form: Form::Annotated,
                writes: writes
                    .into_iter()
                    .map(|(value, index)| Write { value, index })
                    .collect(),
            };
            match conflict {
                Some(conflict) => case.rejection(conflict),
                None => Case::InferredArray(case),
            }
        })
        .boxed()
}

pub(crate) fn contract() -> Contract {
    let inferred_arrays = Case::Command {
        source: include_str!("../cases/inferred_arrays.ts").into(),
        exit: ExitStatus::Success,
    };
    let mut witnesses = vec![
        Witness {
            partition: "uniform-records-and-aliases",
            case: inferred_arrays,
        },
        Witness {
            partition: "dependent-arrays",
            case: Case::Command {
                source: "const a:any[]=[];a.push(1);const b:any[]=[];b.push(a[0]);const c:any[]=[];c.push(b[0]+a.length);console.log(JSON.stringify(c));".into(),
                exit: ExitStatus::Success,
            },
        },
    ];
    for (partition, source) in [
        (
            "mixed-index-writes",
            "const a:any[]=[];for(let i=0;i<4;i++)a.push(i);a[0]={value:7};a[1]='payload';",
        ),
        (
            "conflicting-alias",
            "const a:any[]=[];const alias=a;a.push(1);alias.push('bad');",
        ),
        (
            "conflicting-branch",
            "const a:any[]=[];if(Date.now()>0)a.push(1);else a.push('bad');",
        ),
        (
            "conflicting-conditional-field",
            "const a:any[]=[];a.push({value:Date.now()>0?1:'bad'});",
        ),
        (
            "conflicting-nested-array",
            "const a:any[]=[];a.push({values:[1,'bad']});",
        ),
        (
            "conflicting-function-write",
            "const a:any[]=[];a.push(1);function mutate():void{a.push('bad');}mutate();",
        ),
        (
            "unknown-call",
            "function mutate(a:number[]):void{a.push(2);}const a:any[]=[];a.push(1);mutate(a);",
        ),
        (
            "closure-capture",
            "const a:any[]=[];a.push(1);const mutate=()=>{a.push('bad');};mutate();",
        ),
        (
            "reassigned-alias",
            "const a:any[]=[];let alias=a;a.push(1);alias=[2];alias.push(3);",
        ),
        (
            "stored-alias",
            "const a:any[]=[];a.push(1);const holder={items:a};holder.items.push('bad');",
        ),
        (
            "mutable-element-alias",
            "const a:any[]=[];a.push({tags:['ok']});a[0].tags.push(1);",
        ),
        (
            "returned-alias",
            "function make():number[]{const a:any[]=[];a.push(1);return a;}console.log(make().length);",
        ),
        (
            "no-writes",
            "const a:any[]=[];console.log(JSON.stringify(a));",
        ),
    ] {
        witnesses.push(Witness {
            partition,
            case: Case::Reject {
                source: source.into(),
                diagnostic: REQUIRES_ELEMENT_TYPE.into(),
                variants: vec![],
            },
        });
    }

    witnesses.push(Witness {
        partition: "conditional-temporary",
        case: ArrayCase {
            element: Element::Number,
            seed: 0,
            writes: vec![],
            aliases: 1,
            form: Form::Annotated,
        }
        .rejection(Conflict::Conditional),
    });

    for (partition, element) in [
        ("number-writes", Element::Number),
        ("string-writes", Element::Text),
        ("boolean-writes", Element::Boolean),
        ("record-writes", Element::Record),
        ("nested-array-writes", Element::Array),
    ] {
        witnesses.push(Witness {
            partition,
            case: Case::InferredArray(ArrayCase {
                element,
                seed: 0,
                aliases: 3,
                form: Form::Annotated,
                writes: vec![
                    Write {
                        value: 1,
                        index: None,
                    },
                    Write {
                        value: -2,
                        index: Some(0),
                    },
                    Write {
                        value: 3,
                        index: Some(1),
                    },
                ],
            }),
        });
    }
    Contract::new(
        "ecma.array.inferred_storage",
        witnesses,
        strategy(),
        check_equivalence,
    )
}
