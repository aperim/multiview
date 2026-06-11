// The honest Tier-D latency badge (managed-devices.md §8, ADR-M011): cast
// playback on the Default Media Receiver runs seconds behind live —
// HLS segment-buffered, glass-to-glass typically 6–30 s; LL-HLS does not
// auto-engage there — and the badge says so in text (never colour alone,
// WCAG 1.4.1), with the longer truth in the tooltip.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Timer } from 'lucide-react';

import { Badge } from '../components/ui/badge';

/** The seconds-class latency pill every cast session row carries. */
export function CastLatencyBadge(): JSX.Element {
  const { t } = useLingui();
  return (
    <Badge
      variant="outline"
      title={t`Glass-to-glass latency on the Default Media Receiver is seconds-class — typically 6–30 s — because playback buffers whole HLS segments. This is inherent to casting, not a fault.`}
    >
      <Timer className="size-3.5" aria-hidden="true" />
      <span>
        <Trans>Tier D — 6–30 s behind live</Trans>
      </span>
    </Badge>
  );
}
