// String splitting, joining, replacement, padding, casing, regex search and scalar iteration.

function present(value: string | undefined): number {
  return !value ? 0 : value.length;
}
function splitHeader(header: { value: string }): string[] {
  return header.value.split(",");
}
function headers(n: number): string {
  const parts = splitHeader({ value: ",max-age=1,private," });
  const characters = "é漢".split("");
  const blank = "".split("");
  const whole = "".split(",");
  let result = present(parts[0]) + present(parts[1]) + present(parts[8]);
  if (!n) result += 100;
  const parsed = JSON.parse('[false,true,0,1,"","x",null,[],{}]');
  for (let i = 0; i < 9; i++) {
    if (parsed[i]) result += 1;
  }
  return `${result}:${parts.length}:${characters.length}:${blank.length}:${whole.length}`;
}
for (const n of [0, -0, NaN, 1, -1, Infinity]) console.log("header", headers(n));

function charLoop(s: string): number {
  let count = 0;
  for (let i = 0; i < s.length; i++) {
    if (s[i] === "-") count++;
  }
  return count;
}
const words = "  hello,  world;foo\tbar  ".trim().split(/[,;\s]+/);
const nums: number[] = [1, 2, 3];
const empty: string[] = [];
const optional: Array<string | null | undefined> = ["a", null, undefined, "b"];
console.log("join", words.join("-"), words.join(""), words.join(), nums.join(":"), empty.join(","), optional.join("-"));
console.log("replace", "a-b-c".replace("-", "_"), "a-b-c".replaceAll("-", "_"), "ab".replaceAll("", "-"));
console.log("replace-regex", "hello   world\tfoo".replace(/\s+/, " "), "hello   world\tfoo".replace(/\s+/g, " "), "hello   world\tfoo".replaceAll(/\s+/g, "_"));
console.log("replace-patterns", "abc".replace(/b/, "[$&:$`:$':$$]"), "ab".replace(/(?:)/g, "-"), "Hello HELLO hello".replace(/hello/gi, "hi"));
console.log("split-regex", JSON.stringify("ab".split(/(?:)/)), JSON.stringify("ab".split(/^/)), JSON.stringify("ab".split(/$/)), JSON.stringify("ab".split(/b$/)), JSON.stringify("".split(/$/)));
console.log("trim-pad", "  hi  ".trimStart(), "  hi  ".trimEnd(), "5".padStart(3, "0"), "5".padEnd(3, "0"), "abc".padStart(6, "xy"), "abc".padEnd(6, "xy"));
console.log("at", "é漢z".at(0), "é漢z".at(-2), "é漢z".at(9));
let clean = "";
for (const ch of "a-b c") clean += ch === "-" || ch === " " ? "_" : ch;
console.log("scalars", clean, charLoop("a-b-c"));
console.log("normalize", "é".normalize() === "é", "é".normalize("NFD") === "é");

function captures(): void {
  const rx = /^([a-z]+)-(\d+)$/i;
  const m = rx.exec("AbC-42");
  const matched = " Item-99 ".trim().match(/^item-(\d+)$/i);
  const allNums = "a12b34c56".match(/\d+/g);
  console.log("exec", rx.test("AbC-42"), rx.test("AbC-xx"), m === null ? "none" : m.join("|"), rx.exec("nope") === null);
  console.log("match", matched === null ? "none" : matched.join("|"), allNums === null ? "none" : allNums.join(","));
}
captures();

const scalarText = "é漢abc  ";
for (const x of [0, 1, -0.5, 2.5, -2.5, 7, NaN]) {
  console.log("positions", x, scalarText.charAt(x), scalarText.indexOf("a", x), scalarText.slice(-4, x), scalarText.substring(x, 1), scalarText.toUpperCase().codePointAt(0), scalarText.trim().length, scalarText.lastIndexOf("b"), scalarText.includes("漢"));
}

// Literal search over BMP text: `search` index and `test` per pattern and input.
const searchInputs = ["", "abc", "word words", "_word_", "é漢123", " 42", "a\nb", "a\rz", "x\n", "x\r\n", "a b", "﻿42", "ababcdaaz"];
console.log("a+", searchInputs.map((s) => `${s.search(/a+/)}${/a+/.test(s)}`).join(" "));
console.log("a|é", searchInputs.map((s) => `${s.search(/a|é/u)}${/a|é/u.test(s)}`).join(" "));
console.log("漢.*", searchInputs.map((s) => `${s.search(/漢.*/u)}${/漢.*/u.test(s)}`).join(" "));
console.log("[a-z0-9_]+", searchInputs.map((s) => `${s.search(/[a-z0-9_]+/)}${/[a-z0-9_]+/.test(s)}`).join(" "));
console.log("^a.*z$", searchInputs.map((s) => `${s.search(/^a.*z$/s)}${/^a.*z$/s.test(s)}`).join(" "));
console.log("\\bword\\b", searchInputs.map((s) => `${s.search(/\bword\b/u)}${/\bword\b/u.test(s)}`).join(" "));
console.log("\\s+\\d", searchInputs.map((s) => `${s.search(/\s+\d{1,3}/u)}${/\s+\d{1,3}/u.test(s)}`).join(" "));
console.log("[^\\w]+", searchInputs.map((s) => `${s.search(/[^\w]+/)}${/[^\w]+/.test(s)}`).join(" "));
console.log("(?:ab|cd)", searchInputs.map((s) => `${s.search(/(?:ab|cd){1,3}/)}${/(?:ab|cd){1,3}/.test(s)}`).join(" "));
console.log("a*?", searchInputs.map((s) => `${s.search(/a*?/)}${/a*?/.test(s)}`).join(" "));
console.log("[]", searchInputs.map((s) => `${s.search(/[]/)}${/[]/.test(s)}`).join(" "));
console.log("[^]", searchInputs.map((s) => `${s.search(/[^]/u)}${/[^]/u.test(s)}`).join(" "));
console.log("\\u{6f22}", searchInputs.map((s) => `${s.search(/\u{6f22}/u)}${/\u{6f22}/u.test(s)}`).join(" "));
console.log("x$", searchInputs.map((s) => `${s.search(/x$/)}${/x$/.test(s)}`).join(" "));
console.log("[\\d\\s]+", searchInputs.map((s) => `${s.search(/[\d\s]+/)}${/[\d\s]+/.test(s)}`).join(" "));
console.log("a.b", searchInputs.map((s) => `${s.search(/a.b/)}${/a.b/.test(s)}`).join(" "));

// Unicode casing across final-sigma contexts and every ASCII neighbour.
const casing = ["", "AbC", "Straße", "ﬀﬁﬂ", "İIıi", "ΣΑΣ", "AΣ", "AΣ́", "AΣ́A", "aͅΣ", "𐐀𐐨🙂", "\0aΣ\0", "漢É雪"];
for (let byte = 0; byte < 128; byte++) {
  const scalar = String.fromCodePoint(byte);
  casing.push(`A${scalar}Σ`, `AΣ${scalar}A`, `Σ${scalar}`);
}
for (const input of casing) console.log("case", JSON.stringify([input.toLowerCase(), input.toUpperCase()]));
