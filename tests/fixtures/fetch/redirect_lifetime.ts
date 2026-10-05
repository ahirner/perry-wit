export async function run(base: string): Promise<string> {
  const pending = fetch(base + "/redirect");
  const response = await pending;
  if (!response.redirected || response !== await pending) return "retained response";
  if (response.url !== base + "/release") return "redirect url";
  return await response.text();
}
