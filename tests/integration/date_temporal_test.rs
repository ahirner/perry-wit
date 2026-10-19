use crate::core_program::Program;

#[test]
fn temporal_field_objects_preserve_defaults_overflow_and_evaluation() {
    let source = r#"
interface Fields {
    year: number; month: number; day: number;
    hour?: number; minute?: number; second?: number;
    millisecond?: number; microsecond?: number; nanosecond?: number;
}
function convert(fields: Fields, options: {overflow?: 'constrain'|'reject'}): string {
    return Temporal.PlainDateTime.from(fields, options).toString();
}
export function run(): number {
    if (Temporal.PlainDateTime.from({year:2024, month:2, day:29, hour:12}, {overflow:'reject'}).toString() !== '2024-02-29T12:00:00') return 1;
    if (Temporal.PlainDateTime.from({year:2024, month:2, day:29, hour:undefined}).toString() !== '2024-02-29T00:00:00') return 2;
    if (convert({year:2024, month:2, day:29, minute:undefined}, {}) !== '2024-02-29T00:00:00') return 3;
    if (convert({year:2024, month:2, day:31, hour:24, minute:-1, second:61, millisecond:1000, microsecond:-1, nanosecond:1001}, {overflow:undefined}) !== '2024-02-29T23:00:59.999000999') return 4;
    if (convert({year:2024.9, month:2.9, day:29.9, hour:12.9, minute:34.9, second:56.9, millisecond:123.9, microsecond:456.9, nanosecond:789.9}, {overflow:'reject'}) !== '2024-02-29T12:34:56.123456789') return 5;
    let errors=0;
    try { convert({year:2024, month:2, day:31}, {overflow:'reject'}); } catch(e) { if(e instanceof RangeError) errors++; }
    try { convert({year:2024, month:2, day:29, hour:24}, {overflow:'reject'}); } catch(e) { if(e instanceof RangeError) errors++; }
    try { convert({year:2024, month:2, day:29, minute:-1}, {overflow:'reject'}); } catch(e) { if(e instanceof RangeError) errors++; }
    try { convert({year:2024, month:2, day:29, nanosecond:NaN}, {}); } catch(e) { if(e instanceof RangeError) errors++; }
    try { convert({year:2024, month:2, day:29, second:Infinity}, {}); } catch(e) { if(e instanceof RangeError) errors++; }
    try { Temporal.PlainDateTime.from('2024-02-29', {overflow:'bad'}); } catch(e) { if(e instanceof RangeError) errors++; }
    try { Temporal.PlainDate.from('2024-02-29', {overflow:'bad'}); } catch(e) { if(e instanceof RangeError) errors++; }
    if(errors!==7) return 6;
    if(Temporal.PlainDateTime.from('2024-02-29T12:34:56.123456789', {overflow:'reject'}).toString() !== '2024-02-29T12:34:56.123456789') return 7;
    const trace:number[]=[];
    function mark(n:number):number { trace.push(n); return n; }
    const value=Temporal.PlainDateTime.from({year:mark(2024), ignored:mark(9), month:mark(2), day:mark(31), hour:mark(25), hour:mark(23)}, {ignored:mark(8), overflow:'reject', overflow:'constrain'});
    if(value.toString()!=='2024-02-29T23:00:00' || trace.join(',')!=='2024,9,2,31,25,23,8') return 8;
    return 0;
}
"#;
    let mut program = Program::checked(source, "temporal-fields.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn date_parse_reports_legacy_formats_but_keeps_invalid_iso_nan() {
    let source = r#"
function parse(input:string):number { return Date.parse(input); }
export function run():number {
    let errors=0;
    try { Date.parse('Sun, 06 Nov 1994 08:49:37 GMT'); } catch(e) {
        if(e instanceof RangeError && e.message.includes('Unsupported legacy Date string format')) errors++;
    }
    for(const text of ['Sun, 06 Nov 1994 08:49:37 GMT', '11/06/1994', 'November 6, 1994', 'invalid', '']) {
        try { parse(text); } catch(e) { if(e instanceof RangeError && e.message.includes('use an ISO date')) errors++; }
        try { new Date(text); } catch(e) { if(e instanceof RangeError && e.message.includes('use an ISO date')) errors++; }
    }
    if(errors!==11) return 1;
    for(const text of ['2024-13-01', '2024-01-32', '2024-02-', '2024-01-01T25:00Z', '+275760-09-14']) {
        if(!Number.isNaN(parse(text)) || !Number.isNaN(new Date(text).getTime())) return 2;
    }
    if(parse('1994-11-06T08:49:37Z')!==784111777000) return 3;
    if(new Date('1970-01-01T01:00+01:00').getTime()!==0) return 4;
    if(parse('1970')!==0 || parse('1970-01')!==0 || parse('+001970-01-01')!==0) return 5;
    return 0;
}
"#;
    let mut program = Program::checked(source, "date-legacy.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}
