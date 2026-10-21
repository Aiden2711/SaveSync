/**
 * Deterministic cover art for the library grid.
 *
 * Covers are generated locally from the game title: a stable hash picks a
 * palette and a motif, and the title supplies a small monogram. No network,
 * no images, no randomness, so the same title always renders the same cover.
 */

export interface CoverPalette {
  /** Flat background. */
  bg: string;
  /** Monogram and microtext; AA contrast against `bg`. */
  ink: string;
  /** Large geometric motif. */
  accent: string;
}

/**
 * Six flat palettes drawn from the SaveSync token set. Every `ink` keeps at
 * least 5:1 contrast against its `bg`, so monogram and adapter label stay
 * readable at small cover sizes.
 */
export const COVER_PALETTES: readonly CoverPalette[] = [
  { bg: "#3D4F97", ink: "#F7F3E8", accent: "#E48600" },
  { bg: "#E9E5DB", ink: "#1E2230", accent: "#3D4F97" },
  { bg: "#8BA1D4", ink: "#1E2230", accent: "#FBFAF6" },
  { bg: "#2C3B74", ink: "#F1EEE7", accent: "#9FBEE7" },
  { bg: "#E48600", ink: "#1E2230", accent: "#FBFAF6" },
  { bg: "#DEDEDE", ink: "#A7282B", accent: "#1E2230" },
];

export const COVER_MOTIFS = ["diagonal", "squares", "circle"] as const;
export type CoverMotif = (typeof COVER_MOTIFS)[number];

export const ADAPTER_LABELS: Record<string, string> = {
  custom_directory: "Custom folder",
  ludusavi: "Ludusavi (PC)",
  emulator: "Emulator",
  jksv: "JKSV export",
};

/** Human-readable adapter name; unknown adapters fall back to their raw id. */
export function adapterLabel(adapter: string): string {
  return ADAPTER_LABELS[adapter] ?? adapter.replace(/_/g, " ");
}

/** FNV-1a, 32-bit. Stable across runs and platforms. */
export function fnv1a(value: string): number {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

/**
 * FNV-1a leaves its low bits unevenly distributed for short strings, which
 * made most titles land on the same motif. One avalanche pass keeps the hash
 * deterministic while spreading palette and motif choices.
 */
function mix32(value: number): number {
  let hash = value;
  hash ^= hash >>> 16;
  hash = Math.imul(hash, 0x85ebca6b);
  hash ^= hash >>> 13;
  hash = Math.imul(hash, 0xc2b2ae35);
  hash ^= hash >>> 16;
  return hash >>> 0;
}

/** Words that add no identity to a two-letter monogram. */
const SKIP_WORDS = new Set([
  "the",
  "a",
  "an",
  "of",
  "and",
  "el",
  "la",
  "los",
  "las",
  "le",
  "les",
  "de",
  "del",
  "der",
  "die",
  "das",
  "und",
  "et",
  "y",
]);

/**
 * One or two letters derived from the title: initials of the first two
 * significant words, or the first two characters of a single-word title.
 */
export function monogramFor(title: string): string {
  const folded = title
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "");
  const words = folded
    .split(/\s+/)
    .map((word) => word.replace(/[^\p{L}\p{N}]/gu, ""))
    .filter((word) => word.length > 0)
    .filter((word) => !SKIP_WORDS.has(word.toLowerCase()));

  if (words.length === 0) return "?";
  if (words.length === 1) {
    return Array.from(words[0]).slice(0, 2).join("").toUpperCase();
  }
  const first = Array.from(words[0])[0];
  const second = Array.from(words[1])[0];
  return `${first}${second}`.toUpperCase();
}

export interface CoverSpec {
  palette: CoverPalette;
  motif: CoverMotif;
  monogram: string;
}

export function coverFor(title: string): CoverSpec {
  const hash = mix32(fnv1a(title));
  return {
    palette: COVER_PALETTES[hash % COVER_PALETTES.length],
    motif: COVER_MOTIFS[(hash >>> 8) % COVER_MOTIFS.length],
    monogram: monogramFor(title),
  };
}
