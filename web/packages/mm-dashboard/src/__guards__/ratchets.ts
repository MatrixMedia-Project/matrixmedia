// Source-scan ratchets (master plan §C2). Each constant is the EXACT current count:
// ratchets.test.ts fails when a count rises above its constant, and also when it falls below it
// ("lower the ratchet to N"), so a PR that removes occurrences lowers the constant in the same PR.
// PR 13 sets MAX_HEX_LINES_IN_TSX and MAX_OUTLINE_NONE to 0 and MAX_INLINE_STYLE_LINES to the
// documented dynamic-geometry allowance. Measured on origin/development 4d1aa34.

/** Lines in src/**\/*.tsx (not *.test.tsx) that contain `style={{`. */
export const MAX_INLINE_STYLE_LINES = 451;

/** Lines in src/**\/*.tsx (not *.test.tsx) matching /#[0-9a-fA-F]{3,8}\b/; src/ui/tierColors.* is exempt. */
export const MAX_HEX_LINES_IN_TSX = 90;

/** Occurrences of `outline: none` in src/**\/*.css plus `outline: 'none'` in src/**\/*.tsx (not tests). */
export const MAX_OUTLINE_NONE = 1;
