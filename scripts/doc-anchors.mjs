/** GitHub-style heading IDs for maintained Markdown, excluding fenced examples. */
export function markdownAnchors(text) {
  const prose = text.replace(/```[\s\S]*?```|~~~[\s\S]*?~~~/g, "");
  const used = new Set();
  for (const match of prose.matchAll(/^ {0,3}#{1,6}\s+(.+?)\s*#*\s*$/gm)) {
    const base = match[1].toLowerCase().replace(/<[^>]*>/g, "")
      .replace(/[^\p{L}\p{N}_\- ]/gu, "").replace(/ /g, "-");
    let slug = base, suffix = 0;
    while (used.has(slug)) slug = `${base}-${++suffix}`;
    used.add(slug);
  }
  for (const match of prose.matchAll(/<(?:a|[a-z]+)\b[^>]*\b(?:id|name)=["']([^"']+)["']/gi)) used.add(match[1]);
  return used;
}
