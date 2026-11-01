/**
 * Context-menu geometry.
 *
 * The game menu is positioned from raw pointer coordinates, which can sit
 * anywhere in the window — including under the bottom edge, past the right
 * edge, or up inside the top navigation bar. `clampMenuPosition` keeps the
 * whole menu on screen and below the top bar, and is pure so it can be tested
 * without a layout engine (jsdom has none).
 */

export interface Point {
  x: number;
  y: number;
}

export interface Size {
  width: number;
  height: number;
}

/** Gap kept between the menu and every viewport edge. */
export const MENU_MARGIN = 8;

/**
 * Upper-bound menu size used for clamping. The CSS gives the menu
 * `min-width: 232px` and a scrollable body, so treating these as maxima keeps
 * the clamping conservative: the menu can end up slightly further from an edge
 * than strictly necessary, never off it.
 */
export const MENU_SIZE: Size = { width: 232, height: 260 };

/**
 * Used when the top bar cannot be measured (no layout, e.g. jsdom). The bar is
 * 34px of control plus padding and a hairline border.
 */
export const TOPBAR_FALLBACK_HEIGHT = 52;

/**
 * Clamp a pointer position so a menu of `size` stays fully inside `viewport`,
 * never above `topInset` (the bottom edge of the top bar plus a margin) and
 * never closer than `bottomInset` to the bottom edge (the docked band).
 *
 * `bottomInset` defaults to 0, so callers that only know about the top bar keep
 * the original behaviour. Expressing the band as an inset rather than a shorter
 * viewport matters on short windows: shrinking the viewport collapses the floor
 * and the ceiling onto `topInset` and pushes the menu through the band.
 *
 * When the menu cannot fit — a viewport smaller than the menu plus its insets —
 * the menu is pinned to the top-left margin instead of overflowing the opposite
 * edge.
 */
export function clampMenuPosition(
  point: Point,
  size: Size,
  viewport: Size,
  topInset: number,
  bottomInset = 0,
): Point {
  const minY = Math.max(topInset, MENU_MARGIN);
  const maxX = Math.max(MENU_MARGIN, viewport.width - size.width - MENU_MARGIN);
  const maxY = Math.max(
    minY,
    viewport.height - bottomInset - size.height - MENU_MARGIN,
  );
  return {
    x: Math.min(Math.max(point.x, MENU_MARGIN), maxX),
    y: Math.min(Math.max(point.y, minY), maxY),
  };
}
