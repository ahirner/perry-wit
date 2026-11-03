function sum(x: number, y: number) {
  if (x < 0) return y - x;
  return x + y;
}

export function narrow(x: number, y: number) {
  return sum(x, y);
}

export function wide(x: number, y: number) {
  return sum(x, y);
}

export function floating(x: number, y: number) {
  return sum(x, y);
}
