//! BMP string operations with scalar observations and independent Node comparison.
use crate::{
    execution::check_equivalence,
    registry::{Case, Contract, ExitStatus, Witness},
};
use proptest::prelude::*;

pub fn contract() -> Contract {
    Contract {
        id: "ecma.string.utf8",
        description: "Typed string operations preserve scalar indices, optional results and immutable UTF-8 spans",
        specification: "https://tc39.es/ecma262/#sec-properties-of-the-string-prototype-object",
        domain: "BMP scalars only for Node differential comparison; numeric/undefined arguments, split limits, immutable aliases and algebraic relations. Astral scalar semantics and invalid repeat counts have separate component regressions. Fixed programs with generated data seed P6.5 program search.",
        witnesses: vec![
            Witness {
                partition: "empty-and-undefined",
                case: observations("", "", "undefined", "undefined", 0),
            },
            Witness {
                partition: "unicode-nul-casing",
                case: observations("é\0İΟΣ Σ\u{301}ßﬃ", "é", "-0.5", "NaN", 2),
            },
            Witness {
                partition: "whitespace-and-wrapped-limit",
                case: observations(
                    "\u{feff}\t水\u{85} \u{2028}",
                    "水",
                    "4294967297",
                    "Infinity",
                    3,
                ),
            },
        ],
        strategy: (
            prop::collection::vec(
                prop::sample::select(vec![
                    'a', 'A', 'é', '水', 'Σ', 'İ', 'ß', 'ﬃ', '\u{301}', '\0', ' ', '\n',
                    '\u{feff}', '\u{85}',
                ]),
                0..32,
            ),
            prop::sample::select(vec!["", "a", "é", "水", "Σ", "\0", "aa", "absent"]),
            prop::sample::select(vec![
                "undefined",
                "NaN",
                "Infinity",
                "-Infinity",
                "-33",
                "-1",
                "-0.5",
                "-0",
                "0",
                "1",
                "1.5",
                "3",
                "32",
                "4294967297",
            ]),
            prop::sample::select(vec![
                "undefined",
                "NaN",
                "Infinity",
                "-Infinity",
                "-33",
                "-1",
                "0",
                "1",
                "2.5",
                "33",
            ]),
            0u8..5,
        )
            .prop_map(|(text, needle, index, end, count)| {
                observations(
                    &text.into_iter().collect::<String>(),
                    needle,
                    index,
                    end,
                    count,
                )
            })
            .boxed(),
        check: check_equivalence,
    }
}

/// Sorted observation fields keep native serialization order outside the string contract.
fn observations(text: &str, needle: &str, index: &str, end: &str, count: u8) -> Case {
    let cut = text.chars().count() / 2;
    let text = serde_json::to_string(text).unwrap();
    let needle = serde_json::to_string(needle).unwrap();
    let index = if index == "undefined" {
        "absentNumbers[0]"
    } else {
        index
    };
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
const text={text};const needle={needle};
const absentNumbers:number[]=[];const absentStrings:string[]=[];
const wide=text+'界'+text;
const parsed=JSON.parse(JSON.stringify(text));
const result={{
  absent:text.split(absentStrings[0],{index}),at:text.at({index}),
  end:wide.trimEnd(),ends:wide.endsWith(needle,{end}),
  includes:wide.includes(needle,{index}),index:wide.indexOf(needle,{index}),indexed:text[{index}],
  last:wide.lastIndexOf(needle,{index}),lower:wide.toLowerCase(),
  parsed:parsed.toUpperCase(),point:wide.codePointAt({index}),repeat:text.repeat({count}),
  scalar:wide.slice(text.length,text.length+1),slice:text.slice({index},{end}),split:text.split(needle,{index}),
  start:wide.trimStart(),starts:wide.startsWith(needle,{index}),substring:text.substring({index},{end}),
  trim:wide.trim(),unit:text.charAt({index}),upper:wide.toUpperCase()
}};
if(text.slice(0,{cut})+text.slice({cut})!==text)throw 90;
if(wide.trim().trim()!==wide.trim() || wide.toLowerCase().toLowerCase()!==wide.toLowerCase())throw 91;
if(text.repeat({count}).length!==text.length*{count})throw 92;
let original='a'+text;const prefix=original.slice(0,1);const suffix=original.slice(1);
const grown=suffix+'!';original=original+'?';
if(prefix!=='a' || suffix!==text || grown!==text+'!' || original!=='a'+text+'?')throw 93;
const parts=wide.split('界');
if(parts.length!==2 || parts[0]!==text || parts[1]!==text)throw 94;
console.log(JSON.stringify(result));
"#
        ),
    }
}

/// Rejection remains executable even when the corresponding ECMAScript contracts are exempt.
pub fn exclusions() -> Contract {
    let cases = [
        (
            "json-reviver",
            "JSON.parse('1',(_key,value)=>value)",
            "JSON.parse revivers",
        ),
        (
            "json-replacer",
            "JSON.stringify({x:1},(_key,value)=>value)",
            "JSON.stringify replacers and formatting",
        ),
        (
            "json-property-list",
            "JSON.stringify({x:1},['x'])",
            "JSON.stringify replacers and formatting",
        ),
        (
            "json-formatting",
            "JSON.stringify({x:1},null,2)",
            "JSON.stringify replacers and formatting",
        ),
        (
            "char-code-at",
            "'a'.charCodeAt(0)",
            "charCodeAt is a UTF-16 code-unit API",
        ),
        (
            "from-char-code",
            "String.fromCharCode(65)",
            "fromCharCode is a UTF-16 code-unit API",
        ),
        (
            "empty-from-char-code",
            "String.fromCharCode()",
            "fromCharCode is a UTF-16 code-unit API",
        ),
        (
            "computed-from-char-code",
            "String['fromCharCode']()",
            "fromCharCode is a UTF-16 code-unit API",
        ),
        ("dynamic-regex", "new RegExp('a+')", "dynamic RegExp"),
        ("called-regex", "RegExp('a+')", "dynamic RegExp"),
    ]
    .map(|(partition, expression, diagnostic)| Witness {
        partition,
        case: Case::Reject {
            source: format!("{expression};"),
            diagnostic: diagnostic.into(),
        },
    });
    Contract {
        id: "ecma.subset.exclusions",
        description: "Deferred JSON controls, UTF-16 code-unit APIs and dynamic regexes are diagnosed",
        specification: "TODOs.md#d4-language-subset-static-liveness-first",
        domain: "Compile-time rejection of explicitly excluded APIs; ordinary compact JSON and Unicode scalar string operations remain supported",
        strategy: prop::sample::select(cases.iter().map(|w| w.case.clone()).collect::<Vec<_>>())
            .boxed(),
        witnesses: cases.into(),
        check: check_equivalence,
    }
}
