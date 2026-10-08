//! Input domains are pure strategies; rejection by the compiler is never a filter.
use crate::program::{Binary, Condition, Math, Number, Text};
use proptest::prelude::*;

pub fn numbers(depth: u32) -> BoxedStrategy<Number> {
    let leaf = prop_oneof![
        Just(Number::Input),
        Just(Number::NegativeZero),
        prop::sample::select(vec![0, 1, -1, 31, 4294967295]).prop_map(Number::Integer),
        (-1024i64..=1024).prop_map(Number::Integer),
    ]
    .boxed();
    if depth == 0 {
        return leaf;
    }
    let child = numbers(depth - 1);
    let text = texts(depth - 1);
    let condition = conditions(depth - 1);
    prop_oneof![
        4 => leaf,
        3 => (prop::sample::select(vec![Binary::Add, Binary::Sub, Binary::Mul, Binary::Div, Binary::Rem]), child.clone(), child.clone()).prop_map(|(op,a,b)| Number::Binary(op,Box::new(a),Box::new(b))),
        1 => (condition, child.clone(), child.clone()).prop_map(|(c,a,b)| Number::Choose(Box::new(c),Box::new(a),Box::new(b))),
        1 => child.clone().prop_map(|v| Number::Mark(Box::new(v))),
        1 => (child.clone(),child.clone(),child.clone(),0u8..3).prop_map(|(a,b,c,i)| Number::Array(Box::new(a),Box::new(b),Box::new(c),i)),
        1 => (child.clone(),child.clone()).prop_map(|(a,b)| Number::Field(Box::new(a),Box::new(b))),
        1 => text.clone().prop_map(|v| Number::Length(Box::new(v))),
        1 => child.clone().prop_map(|v| Number::Call(Box::new(v))),
        1 => (prop::sample::select(vec![Math::Floor,Math::Ceil,Math::Trunc,Math::Abs,Math::Round]),child.clone()).prop_map(|(op,v)| Number::Math(op,Box::new(v))),
        1 => (text.clone(),text.clone(),child.clone()).prop_map(|(a,b,c)| Number::IndexOf(Box::new(a),Box::new(b),Box::new(c))),
        1 => child.clone().prop_map(|v| Number::DateTime(Box::new(v))),
        1 => (child.clone(),child.clone(),child.clone(),0u8..3).prop_map(|(a,b,c,i)| Number::ByteGet(Box::new(a),Box::new(b),Box::new(c),i)),
        1 => (text.clone(),child.clone(),child).prop_map(|(a,b,c)| Number::EncodedLength(Box::new(a),Box::new(b),Box::new(c))),
        1 => text.clone().prop_map(|v| Number::JsonLength(Box::new(v))),
        1 => text.prop_map(|v| Number::JsonStoredLength(Box::new(v))),
    ].boxed()
}

pub fn conditions(depth: u32) -> BoxedStrategy<Condition> {
    let literal = any::<bool>().prop_map(Condition::Literal);
    if depth == 0 {
        return literal.boxed();
    }
    let number = numbers(depth - 1);
    let child = conditions(depth - 1);
    prop_oneof![
        literal.boxed(),
        (number.clone(), number.clone())
            .prop_map(|(a, b)| Condition::Less(Box::new(a), Box::new(b)))
            .boxed(),
        (number.clone(), number)
            .prop_map(|(a, b)| Condition::Equal(Box::new(a), Box::new(b)))
            .boxed(),
        (child.clone(), child.clone())
            .prop_map(|(a, b)| Condition::And(Box::new(a), Box::new(b)))
            .boxed(),
        (child.clone(), child.clone())
            .prop_map(|(a, b)| Condition::Or(Box::new(a), Box::new(b)))
            .boxed(),
        child.prop_map(|c| Condition::Not(Box::new(c))).boxed(),
    ]
    .boxed()
}

pub fn texts(depth: u32) -> BoxedStrategy<Text> {
    let leaf = prop::sample::select(vec!["", "a", "  abc  ", "é漢", "é", "a\0b", "\n\t"])
        .prop_map(|s| Text::Literal(s.into()))
        .boxed();
    if depth == 0 {
        return leaf;
    }
    let child = texts(depth - 1);
    prop_oneof![
        leaf,
        (child.clone(), child.clone())
            .prop_map(|(a, b)| Text::Concat(Box::new(a), Box::new(b)))
            .boxed(),
        (conditions(depth - 1), child.clone(), child.clone())
            .prop_map(|(c, a, b)| Text::Choose(Box::new(c), Box::new(a), Box::new(b)))
            .boxed(),
        child.clone().prop_map(|v| Text::Lower(Box::new(v))).boxed(),
        child.clone().prop_map(|v| Text::Upper(Box::new(v))).boxed(),
        (child.clone(), numbers(depth - 1))
            .prop_map(|(v, i)| Text::CharAt(Box::new(v), Box::new(i)))
            .boxed(),
        (child, -3i8..=3, -3i8..=3)
            .prop_map(|(v, a, b)| Text::Slice(Box::new(v), a, b))
            .boxed(),
    ]
    .boxed()
}
