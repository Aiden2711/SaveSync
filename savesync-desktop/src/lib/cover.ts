/**
 * Deterministic cover art for the library grid.
 *
 * SaveSync has no artwork provider wired to the frontend yet, so a library must
 * still read as a game library on first launch. Covers are generated locally
 * from the game title: independent stable hashes pick a light or dark field, a
 * palette within that field, and a crest motif, and the title supplies a
 * monogram. No network, no images, no randomness, so the same title always
 * renders the same cover.
 *
 * The palette family is cool and console-ish on purpose (indigo, steel, teal,
 * navy, plum) with three light palettes mixed in, so a shelf of generated covers
 * looks varied instead of like a row of placeholders.
 */

/**
 * A complete cover colour set. Palettes are curated as whole sets: a light
 * `bg`/`bg2` never borrows a dark palette's `ink`/`accent`, because contrast
 * was verified per palette (ink ≥ 4.5:1 on `bg`). Never mix halves.
 */
export interface CoverPalette {
  /** Which brightness family this whole palette belongs to. */
  field: CoverField;
  /** Primary field. */
  bg: string;
  /** Secondary field: one large flat shape behind the crest. */
  bg2: string;
  /** Monogram; at least 4.5:1 against `bg`. */
  ink: string;
  /** Crest strokes (decorative). */
  accent: string;
}

/** Light-field and dark-field covers, so a shelf is never one flat tone. */
export const COVER_FIELDS = ["light", "dark"] as const;
export type CoverField = (typeof COVER_FIELDS)[number];

export const COVER_PALETTES: readonly CoverPalette[] = [
  { field: "dark", bg: "#2b3568", bg2: "#3a4685", ink: "#eef2ff", accent: "#a8b8ff" },
  { field: "dark", bg: "#33415e", bg2: "#44567a", ink: "#f2f6ff", accent: "#a9c4f5" },
  { field: "dark", bg: "#10424f", bg2: "#175a69", ink: "#eafcff", accent: "#7fe0f0" },
  { field: "dark", bg: "#101a33", bg2: "#1b2a4d", ink: "#eaf1ff", accent: "#88a8ff" },
  { field: "dark", bg: "#3a2340", bg2: "#4d2f56", ink: "#fdf1ff", accent: "#e2a6f0" },
  { field: "dark", bg: "#084a63", bg2: "#0c6383", ink: "#eafaff", accent: "#8fe3ff" },
  { field: "dark", bg: "#37424e", bg2: "#4a5967", ink: "#f1f6fb", accent: "#b6cddd" },
  { field: "light", bg: "#d8e3f0", bg2: "#c3d4e8", ink: "#101720", accent: "#2f5f9e" },
  { field: "light", bg: "#e7eef8", bg2: "#d0ddee", ink: "#141f31", accent: "#2c63a8" },
  { field: "light", bg: "#cbdaea", bg2: "#b1c7dd", ink: "#0f1c2b", accent: "#27578e" },
];

/**
 * Selection pools, derived from the flat table above so a palette can never
 * sit in one field's pool while being labelled the other.
 */
export const COVER_PALETTES_BY_FIELD: Readonly<Record<CoverField, readonly CoverPalette[]>> = {
  light: COVER_PALETTES.filter((palette) => palette.field === "light"),
  dark: COVER_PALETTES.filter((palette) => palette.field === "dark"),
};

/** Crest motifs: concentric rounded squares, concentric circles, or slats. */
export const COVER_MOTIFS = ["rings", "orbit", "slats"] as const;
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
  "in",
  "on",
  "at",
  "to",
  "for",
  "with",
  "into",
  "from",
]);

/**
 * One or two letters derived from the title. A single-word title renders as one
 * large channel letter; a multi-word title takes the first letters of its first
 * and last significant words, so "Ori and the Blind Forest" reads as OF rather
 * than the arbitrary OB of its first two words.
 */
export function monogramFor(title: string): string {
  const folded = title.normalize("NFKD").replace(/[\u0300-\u036f]/g, "");
  const words = folded
    .split(/\s+/)
    .map((word) => word.replace(/[^\p{L}\p{N}]/gu, ""))
    .filter((word) => word.length > 0)
    .filter((word) => !SKIP_WORDS.has(word.toLowerCase()));

  if (words.length === 0) return "?";
  if (words.length === 1) {
    return Array.from(words[0])[0].toUpperCase();
  }
  const first = Array.from(words[0])[0];
  const last = Array.from(words[words.length - 1])[0];
  return `${first}${last}`.toUpperCase();
}

export interface CoverSpec {
  /** Brightness family chosen first; the palette is picked within it. */
  field: CoverField;
  palette: CoverPalette;
  motif: CoverMotif;
  monogram: string;
}

/**
 * Share of titles that get a light-field cover, in percent. Roughly half of a
 * shelf reads light, biased to the dark side because the grid sits on a white
 * surface and the dark covers are the ones that look like game boxes. A title
 * has no knowledge of its neighbours, so this is a per-title probability, not
 * a per-library quota: any library bigger than a handful gets both fields.
 */
const LIGHT_FIELD_PERCENT = 45;

/**
 * Brightness family from its own hash domain. Selecting the field before the
 * palette is what makes light covers reliably present: the old scheme drew one
 * index out of all eight palettes, so a real library skewed onto whatever
 * indices its titles happened to hash to and frequently showed no light cover
 * at all.
 */
export function fieldFor(title: string): CoverField {
  return mix32(fnv1a(`${title}#field`)) % 100 < LIGHT_FIELD_PERCENT ? "light" : "dark";
}

/**
 * Field, palette and motif are hashed separately, from the same title with
 * distinct domain suffixes (`#field`, `#field-palette`, `#motif`). Deriving
 * them from one hash made the choices correlate: in practice a handful of
 * titles collapsed onto the same palette, and one motif dominated the grid.
 * Independent inputs decorrelate them, so the pair looks varied rather than
 * duplicated, while staying deterministic per title. The palette index is
 * taken within the chosen field's pool, so the pool size never biases the
 * field split.
 */
export function coverFor(title: string): CoverSpec {
  const field = fieldFor(title);
  const pool = COVER_PALETTES_BY_FIELD[field];
  const paletteIndex = mix32(fnv1a(`${title}#field-palette`)) % pool.length;
  const motifIndex = mix32(fnv1a(`${title}#motif`)) % COVER_MOTIFS.length;
  return {
    field,
    palette: pool[paletteIndex],
    motif: COVER_MOTIFS[motifIndex],
    monogram: monogramFor(title),
  };
}
