let checksum = 0;
for (let i = 0; i < 12; i++) {
  const object: any = {};
  const alias = object;
  for (let j = 0; j < 20; j++) {
    alias["field_" + j] = i * 20 + j;
  }
  checksum += object["field_0"] + object["field_" + 19];
}
console.log(checksum);

const numbers: any = {};
const alias = numbers;
console.log(alias["missing"] === undefined);
console.log(alias["first"] = 7, numbers.second = 11);
alias.length = 13;
numbers.size = 17;
numbers.groups = 19;
numbers.index = 23;
numbers.input = 29;
numbers["é\0雪"] = 31;
numbers[""] = 37;
console.log(numbers.first, alias.second, numbers.length, numbers.size);
console.log(numbers.groups, numbers.index, numbers.input, numbers["é\0雪"], numbers[""]);
console.log(JSON.stringify(numbers), Object.keys(numbers).length, Array.isArray(numbers));

const dependent: any = {};
dependent["total"] = numbers.first + numbers.second;
const array: any[] = [];
array.push(dependent["total"]);
console.log(JSON.stringify(array));

interface Row { value: number }
const parsed: Row = JSON.parse('{"value":7,"extra":{"keep":[1,"雪"]}}');
const rows: any[] = [];
const byKey: any = {};
rows.push(parsed);
byKey["row"] = parsed;
parsed.value = 9;
console.log(JSON.stringify(rows));
console.log(JSON.stringify(byKey));
const copied: Record<string, Row> = JSON.parse(JSON.stringify(byKey));
console.log(JSON.stringify(copied));
