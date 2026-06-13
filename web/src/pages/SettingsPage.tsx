// Settings — appearance + language controls, the API access token, the
// config-file watch status (ADR-W020), the Boot/Loaded/Running boot model
// (ADR-W022: divergence indicator + confirm-gated revert-to-start /
// promote-to-boot), and a demonstration of the accessible toast notifications.
import { useMemo, useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { createApiClient } from "../api/client";
import { getStoredToken, setStoredToken } from "../api/token";
import {
  listEnrollmentTokens,
  listPairingRequests,
  mintEnrollmentToken,
  pairDevice,
  revokeEnrollmentToken,
} from "../devices/enroll";
import type {
  MintedToken,
  PairingRequestView,
  TokenState,
} from "../devices/enroll";
import { ExportConfigButton } from "../resources/FormControls";
import { HelpLink } from "../components/HelpLink";
import { LocaleSwitcher } from "../components/LocaleSwitcher";
import { PageHeader } from "../components/PageHeader";
import { ThemeToggle } from "../components/ThemeToggle";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../components/ui/dialog";
import { Input } from "../components/ui/input";
import { Label } from "../components/ui/label";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "../components/ui/table";
import { toast } from "../components/ui/use-toast";
import { useActiveLocale } from "../i18n/I18nProvider";
import { formatDateTime } from "../i18n/format";

/**
 * The "Configuration file" card (ADR-W020): whether the boot config file is
 * watched for external edits, the watched path, the last applied/rejected
 * loads, and any sections pending a restart — read from
 * `GET /api/v1/config/watch-status`.
 */
function ConfigWatchCard(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const client = useMemo(() => createApiClient(), []);
  const query = useQuery({
    queryKey: ["config", "watch-status"],
    queryFn: async () => {
      const { data } = await client.GET("/api/v1/config/watch-status");
      if (data === undefined) {
        throw new Error(t`Could not load the configuration file watch status.`);
      }
      return data;
    },
    refetchInterval: 15_000,
  });
  const status = query.data;
  const lastApplied = status?.last_applied ?? null;
  const lastRejected = status?.last_rejected ?? null;
  const path = status?.path ?? null;

  return (
    <Card className="lg:col-span-2">
      <CardHeader>
        <CardTitle>
          <Trans>Configuration file</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            External edits to the boot configuration file hot-reload the parts
            that can apply live; an invalid file changes nothing and is
            reported here and on the health banner. Sections that cannot
            hot-apply stay listed until a restart.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-2 text-sm">
        {status === undefined ? (
          <p className="text-muted-foreground">
            {query.isError ? (
              <Trans>The watch status is unavailable.</Trans>
            ) : (
              <Trans>Loading…</Trans>
            )}
          </p>
        ) : (
          <>
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={status.active ? "default" : "outline"}>
                {status.active ? (
                  <Trans>Watching</Trans>
                ) : (
                  <Trans>Not watched</Trans>
                )}
              </Badge>
              {path !== null && (
                <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                  {path}
                </code>
              )}
            </div>
            {lastApplied !== null && (
              <p>
                <Trans>Last applied:</Trans>{" "}
                {formatDateTime(locale, new Date(lastApplied.at_ms))} —{" "}
                {lastApplied.detail}
              </p>
            )}
            {lastRejected !== null && (
              <p className="text-destructive">
                <Trans>Last rejected:</Trans>{" "}
                {formatDateTime(locale, new Date(lastRejected.at_ms))} —{" "}
                {lastRejected.detail}
              </p>
            )}
            {status.restart_pending.length > 0 && (
              <p>
                <Trans>Restart pending:</Trans>{" "}
                <span className="font-medium">
                  {status.restart_pending.join(", ")}
                </span>
              </p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

function tokenStateBadge(
  state: TokenState,
): "default" | "outline" | "live" | "stale" {
  switch (state) {
    case "pending":
      return "live";
    case "used":
      return "default";
    case "revoked":
    case "expired":
      return "stale";
  }
}

/**
 * The "Display Nodes" card (managed-devices.md §9, DEV-B6): mint a one-time
 * enrollment token (its bearer secret shown ONCE in a copyable field), list and
 * revoke existing tokens, complete a screen pairing by typing back the node's
 * six-character code, and an honest node-appliance note. Admin-only on the
 * server; the UI renders and lets a 403 surface as a toast.
 */
function DisplayNodesCard(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const queryClient = useQueryClient();
  const [ttl, setTtl] = useState<string>("");
  const [minted, setMinted] = useState<MintedToken | undefined>(undefined);
  const [busy, setBusy] = useState<boolean>(false);
  const [pairCode, setPairCode] = useState<string>("");

  const tokens = useQuery({
    queryKey: ["devices", "enrollment-tokens"],
    queryFn: async () => listEnrollmentTokens(),
  });
  const pairing = useQuery<readonly PairingRequestView[]>({
    queryKey: ["devices", "pairing-requests"],
    queryFn: async () => listPairingRequests(),
  });

  const reportError = (title: string) => (error: unknown): void => {
    toast({
      title,
      description: error instanceof Error ? error.message : String(error),
      variant: "destructive",
    });
  };

  const refreshTokens = (): void => {
    void queryClient.invalidateQueries({ queryKey: ["devices", "enrollment-tokens"] });
  };

  const onMint = (): void => {
    const parsed = ttl.trim() === "" ? undefined : Number.parseInt(ttl, 10);
    if (parsed !== undefined && (!Number.isFinite(parsed) || parsed <= 0)) {
      toast({
        title: t`Invalid lifetime`,
        description: t`Enter a positive number of seconds, or leave it blank for the default.`,
        variant: "destructive",
      });
      return;
    }
    setBusy(true);
    mintEnrollmentToken(parsed)
      .then((token): void => {
        setMinted(token);
        setTtl("");
        refreshTokens();
      })
      .catch(reportError(t`Could not mint a token`))
      .finally((): void => {
        setBusy(false);
      });
  };

  const onCopy = (): void => {
    if (minted === undefined) {
      return;
    }
    navigator.clipboard
      .writeText(minted.token)
      .then((): void => {
        toast({
          title: t`Token copied`,
          description: t`Paste it into the node now — it is shown only once.`,
        });
      })
      .catch(reportError(t`Could not copy the token`));
  };

  const onRevoke = (tokenId: string): void => {
    revokeEnrollmentToken(tokenId)
      .then((): void => {
        toast({ title: t`Token revoked` });
        refreshTokens();
      })
      .catch(reportError(t`Could not revoke the token`));
  };

  const onPair = (): void => {
    const code = pairCode.trim();
    if (code === "") {
      return;
    }
    setBusy(true);
    pairDevice(code, undefined, undefined)
      .then((result): void => {
        toast({
          title: t`Node paired`,
          description: t`Bound to device ${result.device_id}.`,
        });
        setPairCode("");
        refreshTokens();
        void queryClient.invalidateQueries({ queryKey: ["devices", "pairing-requests"] });
        void queryClient.invalidateQueries({ queryKey: ["resources", "devices"] });
      })
      .catch(reportError(t`Could not pair the node`))
      .finally((): void => {
        setBusy(false);
      });
  };

  const tokenRows = tokens.data ?? [];
  const pairingRows = pairing.data ?? [];

  return (
    <Card className="lg:col-span-2">
      <CardHeader>
        <CardTitle>
          <Trans>Display Nodes</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            Enroll Multiview display-node appliances. Mint a one-time token and
            present it to the node, or complete the on-screen pairing — then bind
            the node to a head assignment from its device page.
          </Trans>{" "}
          <HelpLink to="/help/display-nodes" label={t`About display nodes`} compact />
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-6">
        {/* Mint + the once-only secret reveal. */}
        <section aria-labelledby="enroll-mint" className="flex flex-col gap-3">
          <h3 id="enroll-mint" className="text-sm font-semibold">
            <Trans>Enrollment tokens</Trans>
          </h3>
          {minted === undefined ? (
            <div className="flex flex-col gap-3 sm:flex-row sm:items-end">
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="token-ttl">
                  <Trans>Lifetime (seconds, optional)</Trans>
                </Label>
                <Input
                  id="token-ttl"
                  inputMode="numeric"
                  placeholder={t`default: 3600`}
                  value={ttl}
                  onChange={(event): void => {
                    setTtl(event.target.value);
                  }}
                  className="sm:w-48"
                />
              </div>
              <Button onClick={onMint} disabled={busy}>
                <Trans>Mint token</Trans>
              </Button>
            </div>
          ) : (
            <div
              role="status"
              className="flex flex-col gap-2 rounded-md border border-primary/40 p-3"
            >
              <p className="text-sm">
                <Trans>
                  Copy this token into the node now — it is shown once and never
                  again.
                </Trans>
              </p>
              <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
                <div className="flex flex-1 flex-col gap-1.5">
                  <Label htmlFor="minted-token">
                    <Trans>Enrollment token</Trans>
                  </Label>
                  <Input
                    id="minted-token"
                    readOnly
                    value={minted.token}
                    onFocus={(event): void => {
                      event.target.select();
                    }}
                  />
                </div>
                <div className="flex gap-2">
                  <Button variant="outline" onClick={onCopy}>
                    <Trans>Copy</Trans>
                  </Button>
                  <Button
                    onClick={(): void => {
                      setMinted(undefined);
                    }}
                  >
                    <Trans>Done</Trans>
                  </Button>
                </div>
              </div>
            </div>
          )}

          {tokenRows.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              <Trans>No enrollment tokens.</Trans>
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>
                    <Trans>Token id</Trans>
                  </TableHead>
                  <TableHead>
                    <Trans>State</Trans>
                  </TableHead>
                  <TableHead>
                    <Trans>Expires</Trans>
                  </TableHead>
                  <TableHead>
                    <span className="sr-only">
                      <Trans>Actions</Trans>
                    </span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {tokenRows.map((token) => (
                  <TableRow key={token.tokenId}>
                    <TableCell className="font-mono text-xs">{token.tokenId}</TableCell>
                    <TableCell>
                      <Badge variant={tokenStateBadge(token.state)}>{token.state}</Badge>
                      {token.usedBy !== undefined ? (
                        <span className="ms-2 text-xs text-muted-foreground">
                          <Trans>by {token.usedBy}</Trans>
                        </span>
                      ) : null}
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {formatDateTime(locale, new Date(token.expiresEpochS * 1000))}
                    </TableCell>
                    <TableCell className="text-end">
                      {token.state === "pending" ? (
                        <Button
                          variant="outline"
                          size="sm"
                          aria-label={`${t`Revoke token`}: ${token.tokenId}`}
                          onClick={(): void => {
                            onRevoke(token.tokenId);
                          }}
                        >
                          <Trans>Revoke</Trans>
                        </Button>
                      ) : null}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </section>

        {/* Screen pairing: the operator types back the node's six-char code. */}
        <section aria-labelledby="enroll-pair" className="flex flex-col gap-3">
          <h3 id="enroll-pair" className="text-sm font-semibold">
            <Trans>Screen pairing</Trans>
          </h3>
          <p className="max-w-prose text-sm text-muted-foreground">
            <Trans>
              A node with no token shows a six-character code on its screen.
              Type it here to complete pairing.
            </Trans>
          </p>
          {pairingRows.length > 0 ? (
            <ul className="flex flex-col gap-1 text-sm">
              {pairingRows.map((req) => (
                <li
                  key={req.fingerprint}
                  className="flex flex-wrap items-center gap-2 rounded-md border p-2"
                >
                  <span className="font-medium">{req.nodeName || req.fingerprint}</span>
                  {req.model !== "" ? (
                    <Badge variant="outline">{req.model}</Badge>
                  ) : null}
                  <span className="text-xs text-muted-foreground">
                    <Trans>awaiting the on-screen code</Trans>
                  </span>
                </li>
              ))}
            </ul>
          ) : null}
          <div className="flex flex-col gap-3 sm:flex-row sm:items-end">
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="pair-code">
                <Trans>Pairing code</Trans>
              </Label>
              <Input
                id="pair-code"
                autoComplete="off"
                placeholder={t`e.g. 7QK2M9`}
                value={pairCode}
                onChange={(event): void => {
                  setPairCode(event.target.value);
                }}
                className="sm:w-48"
              />
            </div>
            <Button onClick={onPair} disabled={busy || pairCode.trim() === ""}>
              <Trans>Pair</Trans>
            </Button>
          </div>
        </section>

        {/* Honest node-appliance section — no fabricated download. */}
        <section aria-labelledby="node-image" className="flex flex-col gap-2">
          <h3 id="node-image" className="text-sm font-semibold">
            <Trans>Node appliance</Trans>
          </h3>
          <p className="max-w-prose text-sm text-muted-foreground">
            <Trans>
              Flash the Multiview node image, point it at this controller's URL,
              and present the minted token (or complete screen pairing). The node
              image is distributed with the release.
            </Trans>
          </p>
          <div>
            <Button variant="outline" disabled aria-disabled="true">
              <Trans>Node image (with the release)</Trans>
            </Button>
          </div>
        </section>
      </CardContent>
    </Card>
  );
}

/** A problem-document error body rendered as a human-readable line. */
function problemText(error: unknown): string {
  if (typeof error === "object" && error !== null) {
    const problem = error as { detail?: unknown; title?: unknown };
    if (typeof problem.detail === "string" && problem.detail !== "") {
      return problem.detail;
    }
    if (typeof problem.title === "string" && problem.title !== "") {
      return problem.title;
    }
  }
  return error instanceof Error ? error.message : String(error);
}

/** The outcome of the last boot-model action, rendered inline in the card. */
type BootActionOutcome =
  | {
      readonly kind: "revert";
      readonly reverted: boolean;
      readonly shed: number;
      readonly summary: readonly string[];
      readonly restartOnly: readonly string[];
    }
  | {
      readonly kind: "promote";
      readonly path: string | null;
      readonly revision: number | null;
    };

/**
 * The "Boot configuration" card (ADR-W022): the Boot/Loaded/Running model from
 * `GET /api/v1/config/boot-model` — per-section divergence of the running
 * state from the startup (Loaded) snapshot and from the boot file on disk —
 * plus the confirm-gated **Revert to start** (`POST
 * /api/v1/config/revert-to-start`, applied live through the one apply
 * machinery) and **Promote to boot** (`POST /api/v1/config/promote`, rewrites
 * the boot configuration file server-side) actions. A run without a boot
 * model (no config file) is reported honestly with no actions.
 */
function BootConfigCard(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const queryClient = useQueryClient();
  const client = useMemo(() => createApiClient(), []);
  const [confirming, setConfirming] = useState<"revert" | "promote" | null>(
    null,
  );
  const [outcome, setOutcome] = useState<BootActionOutcome | null>(null);

  const query = useQuery({
    queryKey: ["config", "boot-model"],
    queryFn: async () => {
      const { data } = await client.GET("/api/v1/config/boot-model");
      if (data === undefined) {
        throw new Error(t`Could not load the boot configuration model.`);
      }
      return data;
    },
    refetchInterval: 15_000,
  });

  const refresh = (): void => {
    // The divergence (and, after a promote, the watch baseline) changed.
    void queryClient.invalidateQueries({ queryKey: ["config"] });
  };

  const revert = useMutation({
    mutationFn: async () => {
      const { data, error } = await client.POST(
        "/api/v1/config/revert-to-start",
        { headers: { "Idempotency-Key": crypto.randomUUID() } },
      );
      if (data === undefined) {
        throw new Error(problemText(error));
      }
      return data;
    },
    onSuccess: (body): void => {
      setOutcome({
        kind: "revert",
        reverted: body.reverted,
        shed: body.shed,
        summary: body.summary,
        restartOnly: body.restart_only,
      });
      refresh();
    },
    onError: (error): void => {
      toast({
        title: t`Could not revert to the start configuration`,
        description: error.message,
        variant: "destructive",
      });
    },
  });

  const promote = useMutation({
    mutationFn: async () => {
      const { data, error } = await client.POST("/api/v1/config/promote", {
        headers: { "Idempotency-Key": crypto.randomUUID() },
      });
      if (data === undefined) {
        throw new Error(problemText(error));
      }
      return data;
    },
    onSuccess: (body): void => {
      setOutcome({
        kind: "promote",
        path: body.path ?? null,
        revision: body.revision ?? null,
      });
      refresh();
    },
    onError: (error): void => {
      toast({
        title: t`Could not promote to the boot configuration file`,
        description: error.message,
        variant: "destructive",
      });
    },
  });

  const model = query.data;
  const divergedLoaded = model?.diverged_from_loaded ?? [];
  const divergedFile = model?.diverged_from_boot_file ?? null;
  const bootFileError = model?.boot_file_error ?? null;
  const resumeFallback = model?.resume_fallback ?? null;
  const activeWrittenAt = model?.active_written_at_ms ?? null;

  return (
    <Card className="lg:col-span-2">
      <CardHeader>
        <CardTitle>
          <Trans>Boot configuration</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            The run started from the boot configuration file (its startup
            snapshot is the revert target) and persists the running state
            continuously. Revert to start re-applies the startup snapshot
            live; promote to boot rewrites the configuration file with the
            current running state.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-2 text-sm">
        {model === undefined ? (
          <p className="text-muted-foreground">
            {query.isError ? (
              <Trans>The boot configuration model is unavailable.</Trans>
            ) : (
              <Trans>Loading…</Trans>
            )}
          </p>
        ) : !model.modeled ? (
          <p className="text-muted-foreground">
            <Trans>
              This run was not started from a configuration file, so there is
              no boot configuration to revert to or promote into.
            </Trans>
          </p>
        ) : (
          <>
            <div className="flex flex-wrap items-center gap-2">
              {typeof model.boot_path === "string" && (
                <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                  {model.boot_path}
                </code>
              )}
              {typeof model.start === "string" && (
                <Badge variant="outline">
                  <Trans>start: {model.start}</Trans>
                </Badge>
              )}
              {model.resumed && (
                <Badge>
                  <Trans>Resumed</Trans>
                </Badge>
              )}
            </div>
            {resumeFallback !== null && (
              <p className="text-destructive">
                <Trans>
                  Resume was requested but fell back to the boot file:
                </Trans>{" "}
                {resumeFallback}
              </p>
            )}
            {divergedLoaded.length > 0 ? (
              <p>
                <Trans>Differs from the startup snapshot:</Trans>{" "}
                <span className="font-medium">{divergedLoaded.join(", ")}</span>
              </p>
            ) : (
              <p className="text-muted-foreground">
                <Trans>In sync with the startup snapshot.</Trans>
              </p>
            )}
            {bootFileError !== null ? (
              <p className="text-destructive">
                <Trans>The boot file cannot be compared:</Trans>{" "}
                {bootFileError}
              </p>
            ) : divergedFile !== null && divergedFile.length > 0 ? (
              <p>
                <Trans>Differs from the boot file:</Trans>{" "}
                <span className="font-medium">{divergedFile.join(", ")}</span>
              </p>
            ) : (
              <p className="text-muted-foreground">
                <Trans>In sync with the boot file.</Trans>
              </p>
            )}
            {activeWrittenAt !== null && (
              <p className="text-muted-foreground">
                <Trans>Running state last persisted:</Trans>{" "}
                {formatDateTime(locale, new Date(activeWrittenAt))}
              </p>
            )}
            {outcome !== null && outcome.kind === "revert" && (
              <p>
                {outcome.reverted ? (
                  <Trans>Reverted to the start configuration.</Trans>
                ) : outcome.shed > 0 ? (
                  <Trans>
                    The revert applied only partially: {outcome.shed} engine
                    command(s) could not be enqueued. Retry once the system
                    settles.
                  </Trans>
                ) : (
                  <Trans>
                    The running state already matched the start configuration;
                    nothing was applied.
                  </Trans>
                )}
                {outcome.summary.length > 0 && (
                  <> {outcome.summary.join("; ")}</>
                )}
                {outcome.restartOnly.length > 0 && (
                  <>
                    {" "}
                    <Trans>Restart required for:</Trans>{" "}
                    {outcome.restartOnly.join(", ")}
                  </>
                )}
              </p>
            )}
            {outcome !== null && outcome.kind === "promote" && (
              <p>
                <Trans>Promoted to the boot configuration file.</Trans>
                {outcome.path !== null && (
                  <>
                    {" "}
                    <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                      {outcome.path}
                    </code>
                  </>
                )}
                {outcome.revision !== null && (
                  <>
                    {" "}
                    <Trans>(revision {outcome.revision})</Trans>
                  </>
                )}
              </p>
            )}
            <div className="flex flex-wrap gap-2 pt-1">
              <Button
                variant="destructive"
                disabled={revert.isPending}
                onClick={(): void => {
                  setConfirming("revert");
                }}
              >
                <Trans>Revert to start</Trans>
              </Button>
              <Button
                variant="outline"
                disabled={promote.isPending}
                onClick={(): void => {
                  setConfirming("promote");
                }}
              >
                <Trans>Promote to boot</Trans>
              </Button>
            </div>
          </>
        )}
      </CardContent>

      <Dialog
        open={confirming === "revert"}
        onOpenChange={(open): void => {
          if (!open) {
            setConfirming(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Revert to start?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                This re-applies the configuration the run started with, live:
                every change made since startup — from the UI, the API, or
                config-file edits — is undone. Sections that cannot apply
                live are reported and re-converge on restart.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setConfirming(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button
              variant="destructive"
              onClick={(): void => {
                setConfirming(null);
                revert.mutate();
              }}
            >
              <Trans>Revert</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog
        open={confirming === "promote"}
        onOpenChange={(open): void => {
          if (!open) {
            setConfirming(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Promote to boot?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                This rewrites the boot configuration file on the server with
                the current running state, so the next start boots into it. A
                config revision is committed for rollback.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setConfirming(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button
              onClick={(): void => {
                setConfirming(null);
                promote.mutate();
              }}
            >
              <Trans>Promote</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </Card>
  );
}

/** The settings page. */
export function SettingsPage(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const [token, setToken] = useState<string>(() => getStoredToken() ?? "");

  const saveToken = (): void => {
    setStoredToken(token);
    // Re-run every query so the pages re-fetch with the new credential.
    void queryClient.invalidateQueries();
    toast({
      title: token === "" ? t`API token cleared` : t`API token saved`,
      description:
        token === ""
          ? t`Requests will be unauthenticated.`
          : t`The UI will authenticate with this token.`,
    });
  };

  return (
    <>
      <PageHeader
        title={<Trans>Settings</Trans>}
        description={<Trans>Appearance and language preferences.</Trans>}
      />

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Appearance</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Choose light, dark, or follow the system.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="flex items-center justify-between gap-4">
            <Label id="theme-label">
              <Trans>Theme</Trans>
            </Label>
            <ThemeToggle />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Language</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Switch the interface language and text direction.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="flex items-center justify-between gap-4">
            <Label>
              <Trans>Locale</Trans>
            </Label>
            <LocaleSwitcher />
          </CardContent>
        </Card>

        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>
              <Trans>API access</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>
                Paste the control-plane bearer token. Set it from the
                MULTIVIEW_CONTROL_TOKEN environment variable, or copy the
                bootstrap token the server logs once at startup. Stored in this
                browser only and sent as a Bearer token to the same-origin API.
              </Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-3 sm:flex-row sm:items-end">
            <div className="flex flex-1 flex-col gap-1.5">
              <Label htmlFor="api-token">
                <Trans>API token</Trans>
              </Label>
              <Input
                id="api-token"
                type="password"
                autoComplete="off"
                placeholder={t`admin.xxxxxxxx-xxxx-…`}
                value={token}
                onChange={(event): void => {
                  setToken(event.target.value);
                }}
              />
            </div>
            <div className="flex gap-2">
              <Button onClick={saveToken}>
                <Trans>Save</Trans>
              </Button>
              <Button
                variant="outline"
                onClick={(): void => {
                  setToken("");
                  setStoredToken("");
                  void queryClient.invalidateQueries();
                  toast({ title: t`API token cleared` });
                }}
              >
                <Trans>Clear</Trans>
              </Button>
            </div>
          </CardContent>
        </Card>

        <ConfigWatchCard />

        <BootConfigCard />

        <DisplayNodesCard />

        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>
              <Trans>Configuration export</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>
                Download the current stores (sources, outputs, overlays,
                layouts, canvas) as multiview.toml. Stored UI edits take effect
                when Multiview restarts with this file; live actions (apply
                layout, swap, routing, salvos) act on the running engine
                immediately.
              </Trans>
            </CardDescription>
          </CardHeader>
          <CardContent>
            <ExportConfigButton />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Notifications</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Accessible toasts announce via a live region.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button
              variant="outline"
              onClick={(): void => {
                toast({
                  title: t`Test notification`,
                  description: t`This toast is announced to assistive technology.`,
                });
              }}
            >
              <Trans>Show a test notification</Trans>
            </Button>
          </CardContent>
        </Card>
      </div>
    </>
  );
}
