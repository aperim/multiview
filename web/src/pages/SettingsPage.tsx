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
import { ExportConfigButton } from "../resources/FormControls";
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
