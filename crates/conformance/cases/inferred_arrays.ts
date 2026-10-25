const items: any[] = [];
const alias = items;
const secondAlias: any[] = alias;
for (let i = 0; i < 8; i++) {
  items.push({
    id: i,
    name: "item_" + i,
    value: i * 3.14159,
    tags: ["tag_" + (i % 10), "tag_" + (i % 5)],
    nested: { x: i, y: i * 2 }
  });
  if (i % 2 === 0) {
    alias[i] = { id: i, name: "even", value: i, tags: ["a", "b"], nested: { x: i, y: i } };
  } else {
    secondAlias[i] = { id: i, name: "odd", value: i, tags: ["c", "d"], nested: { x: i, y: i } };
  }
}
const blob = JSON.stringify(items);
console.log(blob);
console.log(JSON.stringify(JSON.parse(blob)));

function count(n: number): number {
  const words = [];
  words.push("seed");
  const alias = words;
  for (let i = 0; i < n; i++) alias.push("first", "second");
  words[0] = "replacement";
  console.log(JSON.stringify(alias));
  return words.length;
}
console.log(count(3), count(1));

let label: "seed" | "next" = items.length > 0 ? "seed" : "next";
const labels: any[] = [];
labels.push(label);
labels.push("tail");
console.log(JSON.stringify(labels), labels[0].length);

class Counter {
  count(): number {
    const numbers: any[] = [];
    let i = 0;
    do { numbers.push(i); i++; } while (i < 2);
    while (i < 4) { numbers[i] = i; i++; }
    switch (i) {
      case 4: numbers.push(4); break;
      default: numbers.push(5);
    }
    try { numbers.push(6); throw 1; } catch { numbers.push(7); } finally { numbers.push(8); }
    console.log(JSON.stringify(numbers));
    console.log(numbers[20] === undefined, numbers[20] + 1);
    return numbers.length;
  }
}
console.log(new Counter().count());

interface Row { value: number }
function buildMixed(n: number): any[] {
  const values: any[] = [];
  for (let i = 0; i < n; i++) values.push(i);
  values[1] = 'snow 雪';
  values[2] = { value: 7 };
  return values;
}
const values = buildMixed(6);
values[0] = values[0] + 1;
values[1] = values[1] + 1;
console.log(JSON.stringify(values));
console.log(values[0], values[1], values[2].value, values[0].value);
console.log(values[99] === undefined, values[99] + 1, values[1] !== '', values[2] === values[2]);
const row: Row = JSON.parse('{"value":4,"extra":{"keep":[1,"雪"]}}');
const boxed: (number | string | Row)[] = [1, 'x', row];
const held = boxed[2] as Row;
row.value = 8;
console.log(JSON.stringify(boxed), held.value, held === row);
held.value = 9;
console.log(row.value, JSON.stringify(boxed));
const copied: (number | string | Row)[] = JSON.parse(JSON.stringify(boxed));
console.log(JSON.stringify(copied));
const graph: unknown = JSON.parse('[1,"雪",{"value":3,"extra":true}]');
const decoded: (number | string | Row)[] = graph as (number | string | Row)[];
console.log(JSON.stringify(decoded));
