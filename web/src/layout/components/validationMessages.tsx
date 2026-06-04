// Maps a machine ValidationCode to a localized, human-readable message.
//
// The codes are a closed union (see model.ts), so this switch is exhaustive and
// the compiler flags a missing case if a code is added.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import type { ValidationCode } from '../model';

/** A localized message element for a validation code. */
export function ValidationMessage({
  code,
}: {
  readonly code: ValidationCode;
}): JSX.Element {
  switch (code) {
    case 'name-empty':
      return <Trans>A layout name is required.</Trans>;
    case 'canvas-dim':
      return <Trans>Canvas width and height must be positive whole numbers.</Trans>;
    case 'fps-format':
      return (
        <Trans>
          Frame rate must be an exact rational like 30000/1001 — not a decimal.
        </Trans>
      );
    case 'cell-id-empty':
      return <Trans>Every cell needs a non-empty identifier.</Trans>;
    case 'cell-id-duplicate':
      return <Trans>Cell identifiers must be unique within a layout.</Trans>;
    case 'rect-bounds':
      return <Trans>The cell extends outside the canvas.</Trans>;
    case 'rect-extent':
      return <Trans>The cell is too small.</Trans>;
    case 'rotation-range':
      return <Trans>Rotation must be between 0 and 359 degrees.</Trans>;
    case 'no-cells':
      return <Trans>Add at least one cell to the layout.</Trans>;
  }
}
