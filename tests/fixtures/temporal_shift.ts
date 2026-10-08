export function run(value: string, days: number): Result<string, number> {
  try {
    return Temporal.PlainDateTime.from(value).add({ days }).toString();
  } catch (error) {
    if (error instanceof Error && 'code' in error) throw error.code;
    throw error;
  }
}
