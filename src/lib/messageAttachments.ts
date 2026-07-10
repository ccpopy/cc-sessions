export type LocalImageAttachment = {
  name: string;
  path: string;
};

export type ParsedUserMessage = {
  markdown: string;
  images: LocalImageAttachment[];
};

const FILES_HEADING = /^(?:#{1,6}\s*)?Files mentioned by the user:\s*$/im;
const REQUEST_HEADING = /^(?:#{1,6}\s*)?My request for Codex:\s*$/im;

export function parseUserMessageAttachments(text: string): ParsedUserMessage {
  const images: LocalImageAttachment[] = [];
  const seenPaths = new Set<string>();
  const imageBlock = /<image\b([^>]*?)(?:\/\s*>|>\s*<\/image\s*>)/gis;

  let markdown = text.replace(imageBlock, (raw, attributes: string) => {
    const path = attributeValue(attributes, "path");
    if (!path) return raw;

    if (!seenPaths.has(path)) {
      seenPaths.add(path);
      images.push({
        name: attributeValue(attributes, "name") ?? fileName(path) ?? `图片 ${images.length + 1}`,
        path,
      });
    }
    return "";
  });

  const filesHeading = FILES_HEADING.exec(markdown);
  const requestHeading = REQUEST_HEADING.exec(markdown);
  if (
    images.length > 0 &&
    filesHeading &&
    requestHeading &&
    filesHeading.index < requestHeading.index
  ) {
    markdown = markdown.slice(requestHeading.index + requestHeading[0].length);
  }

  return {
    markdown: markdown.replace(/\n{3,}/g, "\n\n").trim(),
    images,
  };
}

function attributeValue(attributes: string, name: string): string | null {
  const pattern = new RegExp(
    `\\b${name}\\s*=\\s*(?:\\[([^\\]]+)\\]|"([^"]*)"|'([^']*)'|([^\\s>]+))`,
    "i",
  );
  const match = pattern.exec(attributes);
  const value = match?.slice(1).find((part) => part !== undefined)?.trim();
  return value ? decodeHtmlEntities(value) : null;
}

function fileName(path: string): string | null {
  const name = path.split(/[\\/]/).pop()?.trim();
  return name || null;
}

function decodeHtmlEntities(value: string): string {
  return value.replace(
    /&(?:amp|quot|apos|lt|gt|#\d+|#x[\da-f]+);/gi,
    (entity) => {
      const lower = entity.toLowerCase();
      const named: Record<string, string> = {
        "&amp;": "&",
        "&quot;": '"',
        "&apos;": "'",
        "&lt;": "<",
        "&gt;": ">",
      };
      if (named[lower]) return named[lower];
      const numeric = lower.startsWith("&#x")
        ? Number.parseInt(lower.slice(3, -1), 16)
        : Number.parseInt(lower.slice(2, -1), 10);
      return Number.isSafeInteger(numeric) && numeric >= 0 && numeric <= 0x10ffff
        ? String.fromCodePoint(numeric)
        : entity;
    },
  );
}
