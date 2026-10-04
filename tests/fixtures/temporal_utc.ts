// Strict UTC interchange validation followed by exact Temporal parsing.
export function run(text: string): Result<string, number> {
  if (text.search(/^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-5][0-9](\.[0-9]{1,9})?Z$/) !== 0) {
    throw 1;
  }
  return Temporal.Instant.from(text).toString();
}
