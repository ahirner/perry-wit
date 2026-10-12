interface Document {
  markdown: string;
  codePoints: number;
}

export function renderMarkdown(title: string, paragraphs: string[]): Document {
  const heading = title.trim();
  let markdown = heading ? "# " + heading : "";
  for (const paragraph of paragraphs) {
    const trimmed = paragraph.trim();
    if (trimmed.length > 0) {
      if (markdown.length > 0) markdown += "\n\n";
      markdown += trimmed;
    }
  }
  return { markdown, codePoints: countCodePoints(markdown) };
}

export function countCodePoints(text: string): number {
  let count = 0;
  for (const symbol of text) count++;
  return count;
}
