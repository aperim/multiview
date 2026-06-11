// In-app guide: casting the program to media renderers (DEV-D3, ADR-M011).
// Section ids are part of the public anchor contract (see
// src/docs/registry.ts). The copy keeps the honest doctrine: server-initiated
// playback, the seconds-class Tier-D latency truth, the cross-VLAN mDNS
// reality, and the real failure modes. Google Cast is a trademark of Google
// LLC; Multiview implements the protocol from open sources and is not
// affiliated with or endorsed by Google.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";

import { PageHeader } from "../../components/PageHeader";
import { Code, DocList, DocSection, Prose } from "./components";

/** Casting guide. */
export function CastingHelpPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Casting</Trans>}
        description={
          <Trans>
            Play the multiview on a Google Cast device — ad hoc in seconds,
            or saved as a managed device that survives restarts.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection id="what-casting-is" title={<Trans>What casting is here</Trans>}>
          <Prose>
            <Trans>
              Casting in Multiview is server-initiated: the control plane
              dials the device's CASTV2 port directly, launches the device's
              built-in Default Media Receiver, and hands it the URL of an HLS
              rendition the engine is already serving. It is not browser tab
              casting — your browser is not involved, nothing is re-encoded,
              and closing the management UI changes nothing. The device pulls
              the same rendition every other HLS client gets
              (encode-once-mux-many), and a cast session is pure control
              plane: the engine never sees it, so program output can never
              depend on it.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              Start one from the Devices page: pick a cast target (an adopted
              cast device, a discovered one, or a manual address), pick the
              rendition, cast. Sessions are ephemeral — they live only in the
              running control plane and are never written to the
              configuration.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="casting-latency" title={<Trans>Latency: honest expectations</Trans>}>
          <Prose>
            <Trans>
              Cast playback is Tier D on the sync ladder: seconds-class
              glass-to-glass latency, typically 6–30 s behind live. The
              Default Media Receiver buffers whole HLS segments before it
              plays, so the floor is several segment durations; LL-HLS does
              not auto-engage there, even when the rendition serves low-latency
              parts. This is inherent to casting, not a fault — use it for
              ambient monitoring on TVs, not for time-critical confidence
              monitoring, and never expect a cast device to join a
              synchronized wall (cast devices are excluded from sync groups).
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="casting-network" title={<Trans>Networks, VLANs, and the manual address</Trans>}>
          <Prose>
            <Trans>
              Discovery finds cast devices via mDNS, which does not cross
              VLANs or subnets by itself. A device that is reachable but on
              another VLAN is simply invisible to a scan — use the manual
              address in the cast sheet instead: <Code>host[:port]</Code>,
              IPv6 literals bracketed, e.g. <Code>[2001:db8::20]:8009</Code>.
              The port defaults to 8009; cast groups advertise a non-default
              port, so give the advertised one for a group.
            </Trans>
          </Prose>
          <Prose>
            <Trans>
              The media URL handed to the device must be reachable FROM the
              device, and cast devices ignore LAN DNS (they resolve via
              hardcoded public resolvers): the server's{" "}
              <Code>control.cast_media_base</Code> must be an IP-literal (or
              publicly resolvable) base of this host on a network the device
              can reach — never a loopback or a <Code>.local</Code> name.
              Starting a session is refused with the reason when no castable
              rendition is configured.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="casting-save-device" title={<Trans>Saving a session as a device</Trans>}>
          <Prose>
            <Trans>
              "Save as device" promotes an ephemeral session into a managed
              cast device: a normal registry entry that config export
              captures, so the assignment survives restarts. The TV keeps
              playing across the promotion — supervision is handed to the
              device's driver without stopping the receiver. From then on it
              behaves like any adopted device: lifecycle states, offline
              alarms, and a rendition assignment the driver re-converges
              whenever the device returns.
            </Trans>
          </Prose>
        </DocSection>

        <DocSection id="casting-failures" title={<Trans>Failure modes</Trans>}>
          <DocList>
            <li>
              <Trans>
                Preempted by another sender: anyone on the network can take
                over a cast device, and Multiview will not fight a person for
                the screen. A preempted session is surfaced as DEGRADED with
                the reason; stop and recast (or re-converge the saved device)
                to reclaim it.
              </Trans>
            </li>
            <li>
              <Trans>
                Device sleep or IP change: cast devices nap and DHCP moves
                them. A saved device is re-resolved by its mDNS identity
                (UUID), not a remembered address, where discovery can see it;
                across VLANs, keep the manual address current. An unreachable
                device rides UNREACHABLE with supervised reconnect and
                backoff — exactly like an input.
              </Trans>
            </li>
            <li>
              <Trans>
                Receiver idles out: if the receiver app exits (the device
                shows its idle screen), the session re-loads the rendition
                rather than counting it as a preemption.
              </Trans>
            </li>
            <li>
              <Trans>
                Program output is unaffected by ALL of these: a cast session
                is control-plane only and can never stall or pace the engine.
              </Trans>
            </li>
          </DocList>
        </DocSection>
      </div>
    </>
  );
}
