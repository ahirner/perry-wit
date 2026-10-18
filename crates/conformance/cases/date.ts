
const d = new Date(1711929600000);
if (d.getTime() === 1711929600000) console.log("EPOCH_OK"); else console.log("EPOCH_FAIL");
console.log(d.toISOString());

const d2 = new Date(Date.now());
if (d2.getTime() > 1700000000000) console.log("DATE_NEW_OK"); else console.log("DATE_NEW_FAIL");

const now = Date.now();
if (now > 1700000000000) console.log("DATE_NOW_OK"); else console.log("DATE_NOW_FAIL");

for (const x of [0, 1, -0.5, 2.5, -2.5, 7, NaN]) {
  try {
    console.log("iso", new Date(x).toISOString());
  } catch (e) {
    console.log("iso", e instanceof RangeError);
  }
}

// UTC parsing, alias-visible mutation, calendar rollover and invalid values.
function inspect(input: string, delta: number): string {
  const date = new Date(input);
  const alias = date;
  const copy = new Date(date);
  const parsed = Date.parse(input);
  const original = date.getTime();
  const year = date.getUTCFullYear();
  const month = date.getUTCMonth();
  const day = date.getUTCDate();
  const weekday = date.getUTCDay();
  const hour = date.getUTCHours();
  const minute = date.getUTCMinutes();
  const second = date.getUTCSeconds();
  const ms = date.getUTCMilliseconds();
  const changed = date.setUTCDate(day + delta);
  const seen = alias.getTime();
  date.setUTCFullYear(2024, 1, 29);
  date.setUTCHours(25, -2, 70, 1001);
  const iso = alias.toISOString();
  const changedMonth = date.setUTCMonth(-1, 32);
  date.setUTCMinutes(63, -4, -1);
  date.setUTCSeconds(61, 1001);
  date.setUTCMilliseconds(-1);
  const final = date.toISOString();
  const utc = Date.UTC(99, -1, 32, 25, 61, 61, 1001);
  const constructed = new Date(99, -1, 32, 25, 61, 61, 1001).getTime();
  date.setTime(-0.4);
  const zero = 1 / date.getTime();
  date.setTime(NaN);
  const invalid = date.setUTCMonth(2);
  const invalidJson = date.toJSON();
  const recovered = date.setUTCFullYear(0);
  const embedded = JSON.stringify([new Date(0), new Date(NaN)]);
  return JSON.stringify({parsed, original, year, month, day, weekday, hour, minute, second, ms, changed, seen, copy: copy.getTime(), iso, changedMonth, final, utc, constructed, zero, invalid, invalidJson, recovered, embedded});
}
const dates: Array<[string, number]> = [
  ["1970-01-01T00:00:00.000Z", -1],
  ["1969-12-31T23:59:59.999Z", 32],
  ["2024-02-29T12:34:56.789+02:30", 365],
  ["2023-02-29", -31],
  ["2000-02-29T24:00Z", 0],
  ["0000-01-01", -366],
  ["-000001-12-31T23:59:59.999Z", 1],
  ["+275760-09-13T00:00:00.000Z", 0],
  ["-271821-04-20T00:00:00.000Z", 0],
  ["invalid", 10],
];
for (const [input, delta] of dates) console.log("utc", inspect(input, delta));
