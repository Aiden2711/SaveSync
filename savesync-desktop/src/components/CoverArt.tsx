import { useMemo } from "react";
import { adapterLabel, coverFor } from "../lib/cover";

const DISPLAY_FONT =
  '"Segoe UI Variable Display", "Segoe UI", system-ui, -apple-system, sans-serif';
const MONO_FONT = '"Cascadia Mono", ui-monospace, monospace';

interface CoverArtProps {
  title: string;
  adapter: string;
}

/**
 * Decorative generated cover: flat ground, one geometric motif, a monogram and
 * the adapter name. Purely presentational — the surrounding button carries the
 * accessible name.
 */
export default function CoverArt({ title, adapter }: CoverArtProps) {
  const { palette, motif, monogram } = useMemo(() => coverFor(title), [title]);
  const adapterName = adapterLabel(adapter).toUpperCase();
  const microtext =
    adapterName.length > 20 ? `${adapterName.slice(0, 19)}…` : adapterName;

  return (
    <span className="cover-art">
      <svg
        viewBox="0 0 300 400"
        preserveAspectRatio="xMidYMid meet"
        aria-hidden="true"
        focusable="false"
      >
        <rect width="300" height="400" fill={palette.bg} />

        {motif === "diagonal" && (
          <polygon points="0,0 300,0 300,96 0,252" fill={palette.accent} />
        )}

        {motif === "squares" && (
          <>
            <rect x="108" y="18" width="200" height="200" fill={palette.accent} />
            <rect x="140" y="50" width="136" height="136" fill={palette.bg} />
            <rect x="172" y="82" width="72" height="72" fill={palette.accent} />
          </>
        )}

        {motif === "circle" && (
          <circle cx="236" cy="92" r="132" fill={palette.accent} />
        )}

        <text
          x="150"
          y="312"
          textAnchor="middle"
          fill={palette.ink}
          fontFamily={DISPLAY_FONT}
          fontSize="120"
          fontWeight="700"
          letterSpacing="-4"
        >
          {monogram}
        </text>

        <line
          x1="26"
          y1="352"
          x2="274"
          y2="352"
          stroke={palette.ink}
          strokeOpacity="0.4"
          strokeWidth="1"
          vectorEffect="non-scaling-stroke"
        />
        <text
          x="26"
          y="374"
          fill={palette.ink}
          fontFamily={MONO_FONT}
          fontSize="12"
          letterSpacing="1.1"
        >
          {microtext}
        </text>

        <rect
          x="0.5"
          y="0.5"
          width="299"
          height="399"
          fill="none"
          stroke={palette.ink}
          strokeOpacity="0.45"
          strokeWidth="1"
          vectorEffect="non-scaling-stroke"
        />
      </svg>
    </span>
  );
}
