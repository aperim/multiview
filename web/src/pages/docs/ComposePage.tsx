// Docs: compose file reference. Mirrors deploy/compose.yaml + the GPU overlays.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";

import { PageHeader } from "../../components/PageHeader";
import {
  Code,
  CodeBlock,
  DocDefinitions,
  DocSection,
  DocTerm,
  Prose,
} from "./components";

/** Compose reference documentation. */
export function ComposePage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Compose reference</Trans>}
        description={
          <Trans>
            The quick-start compose stack, the services it defines, the GPU
            overlays, and how to bring it up and down.
          </Trans>
        }
      />

      <div className="space-y-4">
        <DocSection title={<Trans>The three services</Trans>}>
          <Prose>
            <Trans>
              The quick-start file at <Code>deploy/compose.yaml</Code> brings up
              three services. It is self-contained — no real or private feeds.
            </Trans>
          </Prose>
          <DocDefinitions>
            <DocTerm term={<Code>testsrc</Code>}>
              <Trans>
                A small RTSP server that, on start, runs ffmpeg to publish a
                synthetic test pattern with a sine-wave audio tone to{" "}
                <Code>rtsp://testsrc:8554/test</Code>.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>multiview</Code>}>
              <Trans>
                The engine. It ingests that feed plus three built-in test
                patterns into a 2x2 canvas, encodes once, and writes HLS to a
                named volume.
              </Trans>
            </DocTerm>
            <DocTerm term={<Code>hls</Code>}>
              <Trans>
                A tiny nginx that serves the HLS volume over HTTP so you can open
                the stream in a player at{" "}
                <Code>http://localhost:8888/multiview.m3u8</Code>.
              </Trans>
            </DocTerm>
          </DocDefinitions>
        </DocSection>

        <DocSection title={<Trans>Bring it up and down</Trans>}>
          <CodeBlock label="Shell command">
            {`# start the stack in the background
docker compose -f deploy/compose.yaml up -d

# open http://localhost:8888/multiview.m3u8 in VLC or ffplay

# stop it and remove the named volume
docker compose -f deploy/compose.yaml down -v`}
          </CodeBlock>
          <Prose>
            <Trans>
              The default image encodes <Code>mpeg2video</Code>, which plays in
              VLC and ffplay. For browser playback, switch to the GPL image and an
              H.264 codec (see the config section).
            </Trans>
          </Prose>
        </DocSection>

        <DocSection title={<Trans>GPU overlays</Trans>}>
          <Prose>
            <Trans>
              Compose merges files left to right. The GPU overlay files swap the{" "}
              <Code>multiview</Code> image and add device access; the other
              services are inherited unchanged. Layer an overlay on top of the
              base file:
            </Trans>
          </Prose>
          <CodeBlock label="Shell command">
            {`# NVIDIA
docker compose -f deploy/compose.yaml \\
  -f deploy/compose.gpu-nvidia.yaml up -d

# Intel / AMD (VAAPI) — pass the host render group id
RENDER_GID=$(getent group render | cut -d: -f3) \\
  docker compose -f deploy/compose.yaml \\
  -f deploy/compose.gpu-vaapi.yaml up -d`}
          </CodeBlock>
        </DocSection>

        <DocSection title={<Trans>Exposed ports and roadmap</Trans>}>
          <Prose>
            <Trans>
              The stack publishes the test RTSP feed on <Code>8554</Code> (for
              host inspection) and the HLS HTTP server on <Code>8888</Code>. There
              is no control-API or web-UI port yet: the current binary composites,
              encodes, and writes HLS or file output to disk, but does not yet
              bind a network listener. The live RTSP, NDI, and RTMP output{" "}
              <em>servers</em> and the API listener are on the roadmap.
            </Trans>
          </Prose>
        </DocSection>
      </div>
    </>
  );
}
