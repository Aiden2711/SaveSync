import { describe, expect, it } from "vitest";
import { coverFor, monogramFor } from "./cover";

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

describe("coverFor", () => {
  it("is deterministic per title and picks from the documented palettes", () => {
    const first = coverFor("Hades");
    const second = coverFor("Hades");
    expect(first).toEqual(second);
    expect(first.monogram).toBe("H");
    expect(["rings", "orbit", "slats"]).toContain(first.motif);
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
});
