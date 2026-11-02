import { describe, expect, it } from "vitest";
import {
  ADAPTER_LABELS,
  COVER_MOTIFS,
  COVER_PALETTES,
  COVER_PALETTES_BY_FIELD,
  adapterLabel,
  coverFor,
  fieldFor,
  monogramFor,
} from "./cover";

/** The ten titles pinned as the cover-art regression baseline. */
const LIBRARY = [
  "Hades",
  "Hollow Knight",
  "Celeste",
  "Stardew Valley",
  "Baldur's Gate 3",
  "Elden Ring",
  "Disco Elysium",
  "Hades II",
  "Signalis",
  "Outer Wilds",
] as const;

/**
 * Deterministic synthetic shelf, used for distribution checks: sequential
 * titles plus word combinations, so the hashes see a realistic spread of
 * lengths and characters rather than one numeric pattern.
 */
function syntheticTitles(count: number): string[] {
  const words = ["Hollow", "Knight", "Shadow", "Rune", "Vector", "Isle", "Neon", "Drift", "Iron", "Bloom"];
  const titles: string[] = [];
  for (let index = 0; titles.length < count; index += 1) {
    titles.push(index < count / 2 ? `Title ${index}` : `${words[index % words.length]} ${words[(index * 7 + 3) % words.length]} ${index}`);
  }
  return titles;
}

describe("monogramFor", () => {
  it("renders a single-word title as one channel letter", () => {
    expect(monogramFor("Hades")).toBe("H");
    expect(monogramFor("Celeste")).toBe("C");
  });

  it("takes the first and last significant word's initials", () => {
    expect(monogramFor("Hollow Knight")).toBe("HK");
    expect(monogramFor("Night in the Woods")).toBe("NW");
    expect(monogramFor("Ori and the Blind Forest")).toBe("OF");
    expect(monogramFor("A Short Hike")).toBe("SH");
  });

  it("folds accents to plain letters and never returns an empty mark", () => {
    expect(monogramFor("Pokémon Écarlate")).toBe("PE");
    expect(monogramFor("'-Hades'")).toBe("H");
    expect(monogramFor("!!!")).toBe("?");
  });
});

describe("adapterLabel", () => {
  it("names every adapter the core can emit", () => {
    // Pinned so a new core variant cannot reach the UI as a raw snake_case id.
    expect(adapterLabel("custom_directory")).toBe("Custom folder");
    expect(adapterLabel("ludusavi")).toBe("Ludusavi (PC)");
    expect(adapterLabel("lutris")).toBe("Lutris");
    expect(adapterLabel("steam")).toBe("Steam");
    expect(adapterLabel("emulator")).toBe("Emulator");
    expect(adapterLabel("jksv")).toBe("JKSV export");
    expect(Object.keys(ADAPTER_LABELS).sort()).toEqual([
      "custom_directory",
      "emulator",
      "jksv",
      "ludusavi",
      "lutris",
      "steam",
    ]);
  });

  it("leaves a stored legacy id readable and falls back to the raw id", () => {
    // Lutris and Steam discoveries used to be persisted as "emulator"; the
    // label for that id must keep working for those rows.
    expect(adapterLabel("emulator")).toBe("Emulator");
    expect(adapterLabel("future_adapter")).toBe("future adapter");
  });
});

describe("COVER_PALETTES_BY_FIELD", () => {
  it("partitions the palette table by field without mixing halves", () => {
    const total = COVER_PALETTES_BY_FIELD.light.length + COVER_PALETTES_BY_FIELD.dark.length;
    expect(total).toBe(COVER_PALETTES.length);
    expect(COVER_PALETTES_BY_FIELD.light.every((palette) => palette.field === "light")).toBe(true);
    expect(COVER_PALETTES_BY_FIELD.dark.every((palette) => palette.field === "dark")).toBe(true);
    expect(COVER_PALETTES_BY_FIELD.light.length).toBe(3);
    expect(COVER_PALETTES_BY_FIELD.dark.length).toBe(7);
  });
});

describe("coverFor", () => {
  it("is deterministic per title and picks from the documented palettes", () => {
    const first = coverFor("Hades");
    const second = coverFor("Hades");
    expect(first).toEqual(second);
    expect(first.monogram).toBe("H");
    expect(["rings", "orbit", "slats"]).toContain(first.motif);
  });

  it("is deterministic across repeated calls for every pinned title", () => {
    for (const title of LIBRARY) {
      expect(coverFor(title)).toEqual(coverFor(title));
    }
  });

  it("derives the field from the same hash domain the field helper uses", () => {
    for (const title of [...LIBRARY, ...syntheticTitles(200)]) {
      expect(coverFor(title).field).toBe(fieldFor(title));
      // The palette must come from the chosen field's pool, never the other's.
      expect(coverFor(title).palette.field).toBe(coverFor(title).field);
    }
  });

  it("keeps field and motif independent", () => {
    // Motifs are hashed from their own domain, so a motif must not imply a
    // field: the shelf has to contain both a light and a dark cover sharing
    // one motif, otherwise the two choices have collapsed into one.
    const specs = LIBRARY.map((title) => coverFor(title));
    const shared = COVER_MOTIFS.some((motif) => {
      const fields = new Set(specs.filter((spec) => spec.motif === motif).map((spec) => spec.field));
      return fields.size === 2;
    });
    expect(shared).toBe(true);
    expect(new Set(specs.map((spec) => spec.field)).size).toBe(2);
  });

  it("pins field, palette, motif and monogram for the reference library", () => {
    const expected = [
      ["light", 9, "rings", "H"],
      ["dark", 2, "orbit", "HK"],
      ["dark", 2, "slats", "C"],
      ["light", 7, "orbit", "SV"],
      ["dark", 6, "rings", "B3"],
      ["dark", 5, "rings", "ER"],
      ["dark", 1, "slats", "DE"],
      ["dark", 4, "orbit", "HI"],
      ["light", 8, "slats", "S"],
      ["dark", 0, "slats", "OW"],
    ];
    expect(
      LIBRARY.map((title) => {
        const spec = coverFor(title);
        return [spec.field, COVER_PALETTES.indexOf(spec.palette), spec.motif, spec.monogram];
      }),
    ).toEqual(expected);
  });

  it("decorrelates palette and motif across a small library", () => {
    const titles = [
      "Hades",
      "Hollow Knight",
      "Celeste",
      "Stardew Valley",
      "Outer Wilds",
      "Tunic",
      "Sable",
    ];
    const specs = titles.map((title) => coverFor(title));
    // A shelf must not collapse onto one colour or one motif.
    expect(new Set(specs.map((spec) => spec.palette.bg)).size).toBeGreaterThan(2);
    expect(new Set(specs.map((spec) => spec.motif)).size).toBeGreaterThan(1);
  });

  it("spreads palettes better than one index over the whole table", () => {
    // The previous scheme drew one index over all eight palettes: four of these
    // ten titles landed on palette 0. Per-field selection caps the worst count.
    const counts = new Map<number, number>();
    for (const title of LIBRARY) {
      const index = COVER_PALETTES.indexOf(coverFor(title).palette);
      counts.set(index, (counts.get(index) ?? 0) + 1);
    }
    expect(Math.max(...counts.values())).toBeLessThanOrEqual(3);
  });

  it("balances light and dark fields over a synthetic shelf", () => {
    const titles = syntheticTitles(3000);
    const light = titles.filter((title) => coverFor(title).field === "light").length;
    const share = light / titles.length;
    // Measured 44.4% light on this generator; the band allows hash noise while
    // failing if a single field takes over the shelf.
    expect(share).toBeGreaterThan(0.4);
    expect(share).toBeLessThan(0.6);
  });

  it("uses every palette and every motif over a synthetic shelf", () => {
    const specs = syntheticTitles(3000).map((title) => coverFor(title));
    // Assert on table indices rather than object identity, so the guarantee
    // ("every palette is reachable") does not depend on shared references.
    const paletteIndices = specs.map((spec) => COVER_PALETTES.indexOf(spec.palette));
    expect(paletteIndices).not.toContain(-1);
    expect(new Set(paletteIndices).size).toBe(COVER_PALETTES.length);
    expect(new Set(specs.map((spec) => spec.motif)).size).toBe(COVER_MOTIFS.length);
    // No palette may be starved: the light pools hold 3 of 10 entries, so the
    // least-used palette still clears 5% of the shelf.
    for (const palette of COVER_PALETTES) {
      const count = specs.filter((spec) => spec.palette === palette).length;
      expect(count / specs.length).toBeGreaterThan(0.05);
      expect(count / specs.length).toBeLessThan(0.25);
    }
  });
});
