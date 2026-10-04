export function run(value: string, days: number): Result<string, number> {
  return Temporal.PlainDateTime.from(value).add({ days }).toString();
}
