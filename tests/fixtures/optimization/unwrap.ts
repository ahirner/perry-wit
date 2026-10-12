interface Point { x: number; y: number }

export function run(n: number): number {
  const points: Point[] = [{ x: n, y: 2 * n }];
  return points[0].x + points[0].y;
}
