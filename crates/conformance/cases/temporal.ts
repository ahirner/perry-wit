// Temporal PlainDate, PlainDateTime and Instant arithmetic, comparison, fields, timers and errors.
import { setTimeout } from "node:timers/promises";

const leap = Temporal.PlainDate.from("2024-02-29");
console.log("plain-date", leap.subtract({ days: 1 }).toString(), leap.add({ days: 1 }).toString(), leap.year, leap.month, leap.day, leap.dayOfWeek);
console.log("months", leap.add({ months: 1 }).toString(), leap.until(Temporal.PlainDate.from("2024-05-31"), { largestUnit: "months" }).months);
console.log("plain-date-time", Temporal.PlainDateTime.from("2024-03-01T12:30:00").subtract({ days: 2 }).toString());
const parsedInstant = Temporal.Instant.from("1970-01-01T00:00:02.500Z");
console.log("instant", parsedInstant.epochMilliseconds, parsedInstant.toString());
console.log("offset", Temporal.Instant.from("2024-01-01T00:00:00+01:00").toString());

const d1 = Temporal.PlainDate.from({ year: 2024, month: 2, day: 29 });
const d2 = Temporal.PlainDate.from({ year: 2024, month: 2, day: 31 }, { overflow: "constrain" });
const d3 = Temporal.PlainDate.from("2024-03-10");
console.log("fields", d1.toString(), d2.toString(), d1.until(d3).days, d3.until(d1).days);
console.log("compare", Temporal.PlainDate.compare(d1, d2), Temporal.PlainDate.compare(d1, d3), Temporal.PlainDate.compare(d3, d1));
for (const largestUnit of ["day", "weeks", "months", "years", "invalid"]) {
  try {
    // @ts-expect-error Exercise runtime validation of dynamic unit strings.
    const duration = d1.until(Temporal.PlainDate.from("2025-05-31"), { largestUnit });
    console.log("duration", largestUnit, duration.years, duration.months, duration.weeks, duration.days, duration.sign, duration.blank);
  } catch (e) {
    console.log("invalid-unit", e instanceof RangeError);
  }
}

// Values survive timer payloads; JSON and string forms agree.
const instant = Temporal.Instant.fromEpochMilliseconds(-1);
const date = Temporal.PlainDate.from("2024-02-28").add({ days: 1 });
const dateTime: Temporal.PlainDateTime = Temporal.PlainDateTime.from("2024-02-28T12:30:00.123456789").add({ days: 1 });
const observed = setTimeout(1, instant);
await setTimeout(2);
console.log("timer", (await observed).epochMilliseconds, instant.toJSON() === instant.toString(), date.toJSON() === date.toString(), dateTime.toJSON() === dateTime.toString());
console.log("date-time-fields", date.year, date.month, date.day, date.dayOfWeek, dateTime.hour, dateTime.minute, dateTime.millisecond, dateTime.microsecond, dateTime.nanosecond);
try {
  Temporal.PlainDate.from("x");
} catch (e) {
  console.log("invalid", e instanceof RangeError);
}
try {
  date.add({ days: 1.5 });
} catch (e) {
  console.log("fractional days", e instanceof RangeError);
}
