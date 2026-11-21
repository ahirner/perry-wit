export async function echo(value: string): Promise<string> {
  console.log(value);
  return value;
}
