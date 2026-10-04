// Source port of runner's workflow:calendar/dates@0.1.0 contract.
type IsoDate = [number, number, number, number, number, number, number, number, number, number];
interface Shift { date: IsoDate; days: number; }
type CalendarError = "invalid-date" | "out-of-range";
type OffsetResult = { ok: true; value: IsoDate } | { ok: false; error: CalendarError };

function code(text: string, index: number): number {
  const value = text.codePointAt(index);
  if (value === undefined) { throw 2; }
  return value;
}

export function datesOffset(request: Shift): OffsetResult {
  let date = "";
  let index = 0;
  while (index < request.date.length) {
    date = date + String.fromCodePoint(request.date[index]);
    index = index + 1;
  }
  if (date.search(/^[0-9]{4}-[0-9]{2}-[0-9]{2}$/) !== 0 || date.slice(0, 4) === "0000") {
    return { ok: false, error: "invalid-date" };
  }
  try {
    const parsed = Temporal.PlainDateTime.from(date);
    try {
    const shifted = parsed.add({ days: request.days });
    if (shifted.year < 1 || shifted.year > 9999) {
      return { ok: false, error: "out-of-range" };
    }
    const text = shifted.toString();
    return { ok: true, value: [code(text,0),code(text,1),code(text,2),code(text,3),45,code(text,5),code(text,6),45,code(text,8),code(text,9)] };
    } catch (error) { return { ok: false, error: "out-of-range" }; }
  } catch (error) { return { ok: false, error: "invalid-date" }; }
}
