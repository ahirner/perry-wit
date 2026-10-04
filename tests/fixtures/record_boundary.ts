interface Input { label: string; samples: [number, number, number]; adjust: number; }
type Failure = "empty-label" | "negative";
type Outcome = { ok: true; value: [string, number] } | { ok: false; error: Failure };

export function transformSummarize(input: Input): Outcome {
  if (input.label === "") { return { ok: false, error: "empty-label" }; }
  let total = input.adjust;
  let index = 0;
  while (index < input.samples.length) {
    total = total + input.samples[index];
    index = index + 1;
  }
  if (total < 0) { return { ok: false, error: "negative" }; }
  return { ok: true, value: [input.label + "!", total] };
}
