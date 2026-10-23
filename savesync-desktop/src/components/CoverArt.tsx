import { memo, useMemo } from "react";
import { coverFor } from "../lib/cover";

const MONO_FONT = '"Cascadia Mono", ui-monospace, monospace';

/**
 * Emblem centre. Every motif is stroked around this point so the three
 * variants share one optical size and sit clear of the inner frame.
 */
const EMBLEM_X = 150;
const EMBLEM_Y = 188;

interface CoverArtProps {
  title: string;
  adapter: string;
}

/**
 * Decorative generated cover: a flat cartridge label with an inner frame, one
 * stroked emblem and a small catalog-code monogram. The adapter is not drawn —
 * it is already shown under the card and in the detail meta — but it is kept on
 * the wrapper as a data attribute for styling hooks.
 *
 * Purely presentational: the surrounding button carries the accessible name.
 */
function CoverArt({ title, adapter }: CoverArtProps) {
  const { palette, motif, monogram } = useMemo(() => coverFor(title), [title]);

  const emblemStroke = {
    fill: "none",
    stroke: palette.accent,
    strokeWidth: 2,
    vectorEffect: "non-scaling-stroke",
    strokeLinecap: "round",
  } as const;

  return (
    <span className="cover-art" data-adapter={adapter}>
      <svg
        viewBox="0 0 300 400"
        preserveAspectRatio="xMidYMid meet"
        aria-hidden="true"
        focusable="false"
      >
        <rect width="300" height="400" fill={palette.bg} />

        <rect
          x="20"
          y="20"
          width="260"
          height="360"
          fill="none"
          stroke={palette.ink}
          strokeOpacity="0.18"
          strokeWidth="1"
          vectorEffect="non-scaling-stroke"
        />

        {/*
          Motif geometry stays clear of the viewBox edges. A square rotated 45°
          spans side * √2, so the 74/37 sides read as ~104/52 diamonds and keep
          the mark inside roughly 96..204 horizontally.
        */}
        {motif === "squares" && (
          <>
            <rect
              x={EMBLEM_X - 37}
              y={EMBLEM_Y - 37}
              width="74"
              height="74"
              transform={`rotate(45 ${EMBLEM_X} ${EMBLEM_Y})`}
              {...emblemStroke}
            />
            <rect
              x={EMBLEM_X - 18.5}
              y={EMBLEM_Y - 18.5}
              width="37"
              height="37"
              transform={`rotate(45 ${EMBLEM_X} ${EMBLEM_Y})`}
              {...emblemStroke}
            />
          </>
        )}

        {motif === "circle" && (
          <>
            <circle cx={EMBLEM_X} cy={EMBLEM_Y} r="52" {...emblemStroke} />
            <circle cx={EMBLEM_X} cy={EMBLEM_Y} r="26" {...emblemStroke} />
          </>
        )}

        {motif === "diagonal" && (
          <>
            <line x1="108" y1="224" x2="192" y2="140" {...emblemStroke} />
            <line x1="140" y1="224" x2="192" y2="172" {...emblemStroke} />
            <line x1="108" y1="192" x2="160" y2="140" {...emblemStroke} />
          </>
        )}

        <text
          x={EMBLEM_X}
          y="322"
          textAnchor="middle"
          fill={palette.ink}
          fontFamily={MONO_FONT}
          fontSize="44"
          fontWeight="500"
          letterSpacing="6"
        >
          {monogram}
        </text>
      </svg>
    </span>
  );
}

export default memo(CoverArt);
