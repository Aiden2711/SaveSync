# Warm Console

Late-2000s Nintendo (Wii Shop Channel / 3DS HOME Menu) warmth mixed with modern desktop craft: warm dark surfaces, soft rounded corners, hairline rules, one amber accent, subtle grain texture, brief functional motion.

Direction: Warm dark base, neutrals tinted brown/amber, one luminous amber accent, grain overlay.
Best for: SaveSync desktop shell -- library, snapshots, conflicts, settings.

## Palette

| token | hex | role |
|---|---|---|
| base | #24201c | page background (warm dark gray-brown) |
| surface | #2e2a25 | raised panels, cards, table headers |
| line | #463f39 | borders, rules, dividers |
| ink | #e9e5dd | primary text |
| ink-2 | #c9c2b8 | secondary text |
| ink-3 | #a59d93 | meta, captions -- never body text |
| accent | #e8a33d | one accent, under 10% of surface |
| accent-hover | #d19337 (light: #be7819) | primary button hover fill (--accent-ink on it: 6.41:1 dark, 4.76:1 light) |
| accent-ink | #1f1c19 | text and icons on accent |
| focus-ring | #e8a33d (light: #a2640d) | :focus-visible outline (dark: 7.50:1 on base, 6.61:1 on surface; light: 4.23:1 on base, 4.50:1 on surface) |
| accent-text | #e8a33d | accent-hued TEXT at small sizes (dark theme) |
| scrim | rgba(12, 10, 9, 0.55) | modal backdrop (dark theme) |
| danger-ink | #1f1c19 | text on the danger fill (dark theme) |

```contrast
ink on base AA
ink-2 on base AA
ink-3 on base AA-large
ink on surface AA
accent on base UI
accent-ink on accent AA
```

## Typography

| | Web | PDF / slides (metric-safe) |
|---|---|---|
| Display | "Instrument Sans", system-ui, sans-serif | DejaVu Sans Bold |
| Text | "IBM Plex Mono", ui-monospace, monospace | DejaVu Sans Mono |

Scale ratio: 1.250 (major third)

## CSS

```css
:root {
  --base: #24201c; --surface: #2e2a25; --line: #463f39;
  --ink: #e9e5dd; --ink-2: #c9c2b8; --ink-3: #a59d93;
  --accent: #e8a33d; --accent-ink: #1f1c19;
  --font-display: "Instrument Sans", system-ui, sans-serif;
  --font-text: "IBM Plex Mono", ui-monospace, monospace;
  --radius: 8px;
  --shadow-near: 0 1px 2px rgba(0,0,0,.45);
  --shadow-far: 0 8px 24px rgba(0,0,0,.35);
  --dur-fast: 120ms; --dur: 220ms; --dur-slow: 420ms;
  --ease: cubic-bezier(0.2, 0, 0, 1);
  --ease-out: cubic-bezier(0.16, 1, 0.3, 1);
}

.grain::after {
  content: ""; position: fixed; inset: 0; pointer-events: none; z-index: 9999;
  opacity: .03; mix-blend-mode: multiply;
  background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='.8' numOctaves='4'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)'/%3E%3C/svg%3E");
}
@media (prefers-reduced-transparency: reduce) { .grain::after { display: none } }
```

## Notes

The amber accent is the only saturated color -- emphasis and data highlights only, never large fills. The grain overlay gives the warm dark base the slightly textured, non-digital feel of late-2000s console menus without using an image file. ink-3 must never be used for body text (fails 4.5:1 on base; it passes AA-large only at 24px+).