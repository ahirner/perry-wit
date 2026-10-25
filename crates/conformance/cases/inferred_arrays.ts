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
    return numbers.length;
  }
}
console.log(new Counter().count());
