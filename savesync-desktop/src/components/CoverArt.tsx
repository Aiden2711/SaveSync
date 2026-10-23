import { memo, useMemo } from "react";
import { coverFor } from "../lib/cover";

/**
 * Crest centre. Every motif is drawn around this point, and the monogram sits
 * on it, so the three motifs share one optical size and stay centred when the
 * featured tile crops the 2:3 cover to a 16:9 band.
 */
const CREST_X = 300;
const CREST_Y = 436;

interface CoverArtProps {
  title: string;
  adapter: string;
}

/**
 * Generated cover art: a cool two-tone field, one clipped crest motif and the
 * title's monogram set in the display face.
 *
 * Purely presentational — `aria-hidden` — because the surrounding tile button
 * carries the accessible name. `preserveAspectRatio="xMidYMid slice"` lets the
 * same artwork fill a 2:3 tile and a 16:9 featured band without letterboxing.
 */
function CoverArt({ title, adapter }: CoverArtProps) {
  const { palette, motif, monogram } = useMemo(() => coverFor(title), [title]);
  const glyphSize = Array.from(monogram).length > 1 ? 216 : 268;

  return (
    <span className="cover-art" data-adapter={adapter} aria-hidden="true">
      <svg viewBox="0 0 600 900" preserveAspectRatio="xMidYMid slice" focusable="false">
        <rect width="600" height="900" fill={palette.bg} />

        {/* One large flat shape breaks the field into two tones. */}
        <rect
          x="-170"
          y="-210"
          width="580"
          height="560"
          rx="150"
          fill={palette.bg2}
        />
        <rect
          x="330"
          y="600"
          width="380"
          height="380"
          rx="140"
          fill={palette.bg2}
          opacity="0.55"
        />

        <g fill="none" stroke={palette.accent} strokeWidth="2.5">
          {motif === "rings" && (
            <>
              <rect
                x={CREST_X - 232}
                y={CREST_Y - 232}
                width="464"
                height="464"
                rx="128"
                opacity="0.34"
              />
              <rect
                x={CREST_X - 176}
                y={CREST_Y - 176}
                width="352"
                height="352"
                rx="100"
                opacity="0.5"
              />
              <rect
                x={CREST_X - 120}
                y={CREST_Y - 120}
                width="240"
                height="240"
                rx="70"
                opacity="0.7"
              />
            </>
          )}

          {motif === "orbit" && (
            <>
              <circle cx={CREST_X} cy={CREST_Y} r="222" opacity="0.3" />
              <circle cx={CREST_X} cy={CREST_Y} r="166" opacity="0.46" />
              <circle cx={CREST_X} cy={CREST_Y} r="110" opacity="0.68" />
            </>
          )}

          {motif === "slats" && (
            <g transform={`rotate(-18 ${CREST_X} ${CREST_Y})`} strokeLinecap="round">
              <line
                x1={CREST_X - 190}
                y1={CREST_Y - 96}
                x2={CREST_X + 190}
                y2={CREST_Y - 96}
                opacity="0.3"
                strokeWidth="30"
              />
              <line
                x1={CREST_X - 214}
                y1={CREST_Y}
                x2={CREST_X + 214}
                y2={CREST_Y}
                opacity="0.46"
                strokeWidth="30"
              />
              <line
                x1={CREST_X - 190}
                y1={CREST_Y + 96}
                x2={CREST_X + 190}
                y2={CREST_Y + 96}
                opacity="0.68"
                strokeWidth="30"
              />
            </g>
          )}
        </g>

        <text
          x={CREST_X}
          y={CREST_Y}
          textAnchor="middle"
          dominantBaseline="central"
          fill={palette.ink}
          fontFamily='"Fredoka", "Segoe UI Variable Display", "Segoe UI", system-ui, sans-serif'
          fontSize={glyphSize}
          fontWeight="600"
          letterSpacing="-8"
        >
          {monogram}
        </text>

        <rect
          x="28"
          y="28"
          width="544"
          height="844"
          rx="26"
          fill="none"
          stroke={palette.ink}
          strokeOpacity="0.14"
          strokeWidth="1.5"
        />
      </svg>
    </span>
  );
}

export default memo(CoverArt);
