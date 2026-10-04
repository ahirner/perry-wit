// Source port of the standalone calendar's strict date and checked day-offset contract.
function parseDate(date: string): Temporal.PlainDateTime {
  if (date.length !== 10 || date.search(/^[0-9]{4}-[0-9]{2}-[0-9]{2}$/) !== 0) {
    throw 1;
  }
  if (date.slice(0, 4) === "0000") { throw 1; }
  try { return Temporal.PlainDateTime.from(date); }
  catch (error) { throw 1; }
}

export function run(date: string, days: number): Result<string, number> {
  const original = parseDate(date);
  if (days < -2147483648 || days > 2147483647) {
    throw 2;
  }
  const shifted = original.add({ days });
  if (shifted.year < 1 || shifted.year > 9999) { throw 2; }
  return shifted.toString().slice(0, 10);
}
