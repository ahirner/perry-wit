export function runTask(input: string): string {
  let result = input;
  for (let index = 0; index < 64; index++) {
    result = result + "!";
  }
  return result;
}
