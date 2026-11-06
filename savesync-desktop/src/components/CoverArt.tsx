import { memo, useId, useMemo, useState } from "react";
import { coverFor } from "../lib/cover";

/**
 * Crest geometry shared by both cover shapes.
 *
 * Every motif is drawn around a crest centre, and the monogram sits on it, so
 * the three motifs keep one optical size. The featured banner uses the same
 * centre at a smaller scale, which is why a title's generated art reads as the
 * same cover in the 2:3 tile and in the wide promoted tile.
 */
interface ArtGeometry {
  width: number;
  height: number;
  crestX: number;
  crestY: number;
  /** Motif scale; portrait is 1 so its crest keeps its original geometry. */
  motifScale: number;
  glyphOne: number;
  glyphTwo: number;
  glyphTracking: number;
}

/** 2:3 library/detail cover. Unchanged from the original generated art. */
const PORTRAIT: ArtGeometry = {
  width: 600,
  height: 900,
  crestX: 300,
  crestY: 436,
  motifScale: 1,
  glyphOne: 268,
  glyphTwo: 216,
  glyphTracking: -8,
};

/**
 * 920x430 banner cover for the featured tile (the SteamGridDB wide shape).
 * The crest sits in the left third; the two flat field shapes stay on the
 * right so the monogram is always drawn on the verified ink-on-`bg` pair.
 */
const WIDE: ArtGeometry = {
  width: 920,
  height: 430,
  crestX: 286,
  crestY: 215,
  motifScale: 0.72,
  glyphOne: 206,
  glyphTwo: 162,
  glyphTracking: -6,
};

interface CoverArtProps {
  title: string;
  adapter: string;
  /** Absolute URL of a cached cover, or nullish to draw the generated art. */
  coverUrl?: string | null;
  /**
   * Draw the 920x430 banner rather than the 2:3 portrait. Only the featured
   * tile sets this; every other call site keeps the portrait default.
   */
  wide?: boolean;
}

/**
 * Generated cover art: a cool two-tone field, one clipped crest motif and the
 * title's monogram set in the display face.
 *
 * Purely presentational — `aria-hidden` — because the surrounding tile button
 * carries the accessible name. The `wide` variant reuses the exact palette,
 * motif and monogram selection (`coverFor`) on the 920x430 banner viewBox.
 *
 * Kept separate from {@link CoverArt} so the monogram is not computed at all
 * while a cached cover image is on screen.
 */
const GeneratedArtwork = memo(function GeneratedArtwork({
  title,
  wide,
}: {
  title: string;
  wide: boolean;
}) {
  const { palette, motif, monogram } = useMemo(() => coverFor(title), [title]);
  const geometry = wide ? WIDE : PORTRAIT;
  const glyphSize =
    Array.from(monogram).length > 1 ? geometry.glyphTwo : geometry.glyphOne;
  /** Motif lengths and radii, scaled from the portrait crest. */
  const crest = (value: number) => value * geometry.motifScale;

  /**
   * Per-instance gradient ids. The same title can be on screen twice (library
   * tile and detail cover) and SVG ids are document-global, so a shared id
   * would make the second cover resolve the first one's gradient. `useId`
   * returns a React-shaped value (`:r0:`), reduced here to id-safe characters.
   */
  const instanceId = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const washId = `cover-wash-${instanceId}`;
  const glossId = `cover-gloss-${instanceId}`;

  /**
   * Sheen tone, taken from the palette itself: ink on a light field (a faint
   * grey gloss), bg on a dark field (a faint shadowed top edge).
   */
  const sheen = palette.field === "light" ? palette.ink : palette.bg;

  return (
    <svg
      viewBox={`0 0 ${geometry.width} ${geometry.height}`}
      preserveAspectRatio="xMidYMid slice"
      focusable="false"
    >
      <defs>
        {/*
         * Vertical wash: the field holds flat `bg` through the upper half and
         * settles toward `bg2` at the bottom, so the cover reads as a lit
         * surface instead of a flat poster.
         */}
        <linearGradient id={washId} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor={palette.bg} />
          <stop offset="0.5" stopColor={palette.bg} />
          <stop offset="1" stopColor={palette.bg2} />
        </linearGradient>
        {/* Top gloss: a diagonal sheen fading out before the crest. */}
        <linearGradient id={glossId} x1="0" y1="0" x2="0.55" y2="1">
          <stop offset="0" stopColor={sheen} stopOpacity="0.16" />
          <stop offset="0.42" stopColor={sheen} stopOpacity="0" />
        </linearGradient>
      </defs>

      <rect
        width={geometry.width}
        height={geometry.height}
        fill={`url(#${washId})`}
      />

      {/* One large flat shape breaks the field into two tones. */}
      {wide ? (
        <>
          <rect x="560" y="-180" width="520" height="400" rx="160" fill={palette.bg2} />
          <rect
            x="560"
            y="250"
            width="460"
            height="280"
            rx="140"
            fill={palette.bg2}
            opacity="0.55"
          />
        </>
      ) : (
        <>
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
        </>
      )}

      {/*
       * Gloss sits under the crest and the monogram, so the verified ink-on-bg
       * contrast of the glyph is never modified by the sheen.
       */}
      <rect
        width={geometry.width}
        height={wide ? 300 : 520}
        fill={`url(#${glossId})`}
      />

      <g fill="none" stroke={palette.accent} strokeWidth="2.5">
        {motif === "rings" && (
          <>
            <rect
              x={geometry.crestX - crest(232)}
              y={geometry.crestY - crest(232)}
              width={crest(464)}
              height={crest(464)}
              rx={crest(128)}
              opacity="0.34"
            />
            <rect
              x={geometry.crestX - crest(176)}
              y={geometry.crestY - crest(176)}
              width={crest(352)}
              height={crest(352)}
              rx={crest(100)}
              opacity="0.5"
            />
            <rect
              x={geometry.crestX - crest(120)}
              y={geometry.crestY - crest(120)}
              width={crest(240)}
              height={crest(240)}
              rx={crest(70)}
              opacity="0.7"
            />
          </>
        )}

        {motif === "orbit" && (
          <>
            <circle
              cx={geometry.crestX}
              cy={geometry.crestY}
              r={crest(222)}
              opacity="0.3"
            />
            <circle
              cx={geometry.crestX}
              cy={geometry.crestY}
              r={crest(166)}
              opacity="0.46"
            />
            <circle
              cx={geometry.crestX}
              cy={geometry.crestY}
              r={crest(110)}
              opacity="0.68"
            />
          </>
        )}

        {motif === "slats" && (
          <g
            transform={`rotate(-18 ${geometry.crestX} ${geometry.crestY})`}
            strokeLinecap="round"
          >
            <line
              x1={geometry.crestX - crest(190)}
              y1={geometry.crestY - crest(96)}
              x2={geometry.crestX + crest(190)}
              y2={geometry.crestY - crest(96)}
              opacity="0.3"
              strokeWidth={crest(30)}
            />
            <line
              x1={geometry.crestX - crest(214)}
              y1={geometry.crestY}
              x2={geometry.crestX + crest(214)}
              y2={geometry.crestY}
              opacity="0.46"
              strokeWidth={crest(30)}
            />
            <line
              x1={geometry.crestX - crest(190)}
              y1={geometry.crestY + crest(96)}
              x2={geometry.crestX + crest(190)}
              y2={geometry.crestY + crest(96)}
              opacity="0.68"
              strokeWidth={crest(30)}
            />
          </g>
        )}
      </g>

      <text
        x={geometry.crestX}
        y={geometry.crestY}
        textAnchor="middle"
        dominantBaseline="central"
        fill={palette.ink}
        fontFamily='"Fredoka", "Segoe UI Variable Display", "Segoe UI", system-ui, sans-serif'
        fontSize={glyphSize}
        fontWeight="600"
        letterSpacing={geometry.glyphTracking}
      >
        {monogram}
      </text>

      <rect
        x="12"
        y="12"
        width={geometry.width - 24}
        height={geometry.height - 24}
        rx={wide ? 34 : 42}
        fill="none"
        stroke={palette.ink}
        strokeOpacity="0.07"
        strokeWidth="1.5"
      />

      <rect
        x="28"
        y="28"
        width={geometry.width - 56}
        height={geometry.height - 56}
        rx={wide ? 22 : 26}
        fill="none"
        stroke={palette.ink}
        strokeOpacity="0.14"
        strokeWidth="1.5"
      />
    </svg>
  );
});

/**
 * Cover art slot: a cached image when one is available, otherwise the
 * generated monogram. Both fill the same `aria-hidden` box, so the tile's
 * accessible name is unaffected. `wide` selects the banner shape and its
 * generated fallback; the portrait path is unchanged.
 */
function CoverArt({ title, adapter, coverUrl, wide = false }: CoverArtProps) {
  /**
   * URL whose image failed to load. Storing the URL rather than a boolean means
   * a new `coverUrl` is retried automatically: a successful re-fetch is never
   * stuck on the fallback, and a broken cover never leaves an empty box.
   */
  const [failedUrl, setFailedUrl] = useState<string | null>(null);

  return (
    <span className="cover-art" data-adapter={adapter} aria-hidden="true">
      {coverUrl && coverUrl !== failedUrl ? (
        <img
          className="cover-art-image"
          src={coverUrl}
          alt=""
          aria-hidden="true"
          draggable={false}
          decoding="async"
          onError={() => setFailedUrl(coverUrl)}
        />
      ) : (
        <GeneratedArtwork title={title} wide={wide} />
      )}
    </span>
  );
}

export default memo(CoverArt);
