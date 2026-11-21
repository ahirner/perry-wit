async function size(url: string): Promise<number> {
  const response = await fetch(url);
  return (await response.bytes()).length;
}

export async function sizes(first: string, second: string): Promise<number> {
  const lengths = await Promise.all([size(first), size(second)]);
  return lengths[0] + lengths[1];
}
