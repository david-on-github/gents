import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));

import type {
  DesktopApiAdapter,
  DesktopClientSnapshot,
} from "@source-inc/gents-desktop-client";
import type { Shell } from "../src/ui/hooks/useShell";
import { SetupScreen, type ProviderId } from "../src/ui/screens/setup/SetupScreen";
import { createDesktopUiHarness } from "./ui-harness/desktopHarness";

const AGENT = "did:key:z6MkBombadilAgent";

/* The harness adapter without a managed runtime to gate on, with every call
   observable. */
function setup(
  overrides: Partial<DesktopApiAdapter> = {},
  harness = createDesktopUiHarness({ scenario: "empty-fleet" }),
) {
  const api = {
    ...harness.adapter,
    managedServerStatus: undefined,
    ...overrides,
  } as DesktopApiAdapter;
  for (const name of [
    "codexLogin",
    "claudeLogin",
    "grokLogin",
    "applyConfigComponents",
  ] as const)
    vi.spyOn(api, name);
  const shell = {
    api,
    snapshot: null,
    deployments: [],
    selectedDeployment: null,
    refreshSnapshot: vi.fn().mockResolvedValue(undefined),
    applyConfig: (run: (bridge: DesktopApiAdapter) => Promise<unknown>) => run(api),
  } as unknown as Shell;
  return { api, shell };
}

const login = (api: DesktopApiAdapter, provider: ProviderId) =>
  provider === "openai"
    ? api.codexLogin
    : provider === "anthropic"
      ? api.claudeLogin
      : api.grokLogin;

afterEach(() => vi.useRealTimers());

describe("setup provider sign-in", () => {
  it.each(["openai", "anthropic", "grok"] as const)(
    "starts the %s sign-in when its add-backend form opens",
    async (provider) => {
      const { api, shell } = setup();
      render(
        <SetupScreen
          shell={shell}
          initialStep="inference"
          purpose="add-backend"
          provider={provider}
          agentDid={AGENT}
          onCancel={vi.fn()}
          onDone={vi.fn()}
        />,
      );
      expect(await screen.findByText("Account connected")).toBeVisible();
      expect(login(api, provider)).toHaveBeenCalledTimes(1);
      expect(login(api, provider)).toHaveBeenCalledWith(AGENT);
    },
  );

  it("starts sign-in for a chosen provider, not for the preselected one", async () => {
    const { api, shell } = setup();
    render(
      <SetupScreen
        shell={shell}
        initialStep="inference"
        agentDid={AGENT}
        onDone={vi.fn()}
      />,
    );
    expect(await screen.findByRole("button", { name: "Sign in" })).toBeVisible();
    expect(api.codexLogin).not.toHaveBeenCalled();

    await userEvent.click(screen.getByTestId("setup-provider-anthropic"));
    expect(await screen.findByText("Account connected")).toBeVisible();
    expect(api.claudeLogin).toHaveBeenCalledTimes(1);
    expect(api.codexLogin).not.toHaveBeenCalled();
  });

  it("starts sign-in when the preselected provider is chosen", async () => {
    const { api, shell } = setup();
    render(
      <SetupScreen
        shell={shell}
        initialStep="inference"
        agentDid={AGENT}
        onDone={vi.fn()}
      />,
    );
    await userEvent.click(await screen.findByTestId("setup-provider-openai"));
    expect(await screen.findByText("Account connected")).toBeVisible();
    expect(api.codexLogin).toHaveBeenCalledTimes(1);
  });

  it("keeps a choice made before the account lookup resolves, once", async () => {
    let resolveAccounts: (accounts: []) => void = () => {};
    const { api, shell } = setup({
      listProviderAccounts: vi.fn(
        () => new Promise<[]>((resolve) => (resolveAccounts = resolve)),
      ),
      cancelClaudeLogin: vi.fn().mockResolvedValue(undefined),
    });
    let rejectLogin: (cause: Error) => void = () => {};
    vi.mocked(api.claudeLogin).mockImplementation(
      () => new Promise((_, reject) => (rejectLogin = reject)),
    );
    render(
      <SetupScreen
        shell={shell}
        initialStep="inference"
        purpose="add-backend"
        provider="anthropic"
        agentDid={AGENT}
        onCancel={vi.fn()}
        onDone={vi.fn()}
      />,
    );
    await userEvent.click(await screen.findByRole("button", { name: "Sign in" }));
    resolveAccounts([]);
    rejectLogin(new Error("Claude sign-in was cancelled"));
    expect(await screen.findByRole("alert")).toHaveTextContent("cancelled");
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Sign in" })).toBeEnabled(),
    );
    expect(api.claudeLogin).toHaveBeenCalledTimes(1);
  });

  it("starts sign-in for the preselected provider chosen during the account lookup", async () => {
    let resolveAccounts: (accounts: []) => void = () => {};
    const { api, shell } = setup({
      listProviderAccounts: vi.fn(
        () => new Promise<[]>((resolve) => (resolveAccounts = resolve)),
      ),
    });
    render(
      <SetupScreen
        shell={shell}
        initialStep="inference"
        agentDid={AGENT}
        onDone={vi.fn()}
      />,
    );
    await userEvent.click(await screen.findByTestId("setup-provider-openai"));
    expect(api.codexLogin).not.toHaveBeenCalled();
    resolveAccounts([]);
    expect(await screen.findByText("Account connected")).toBeVisible();
    expect(api.codexLogin).toHaveBeenCalledTimes(1);
  });

  it("leaves sign-in to a click when the account lookup fails", async () => {
    const { api, shell } = setup({
      listProviderAccounts: vi.fn().mockRejectedValue(new Error("runtime not serving")),
    });
    render(
      <SetupScreen
        shell={shell}
        initialStep="inference"
        purpose="add-backend"
        provider="anthropic"
        agentDid={AGENT}
        onCancel={vi.fn()}
        onDone={vi.fn()}
      />,
    );
    await userEvent.click(await screen.findByRole("button", { name: "Sign in" }));
    expect(await screen.findByText("Account connected")).toBeVisible();
    expect(api.claudeLogin).toHaveBeenCalledTimes(1);
  });

  it("does not sign in again to a provider that is already connected", async () => {
    const { api, shell } = setup({
      listProviderAccounts: vi.fn().mockResolvedValue([
        {
          credentialId: "credential-claude",
          agentDid: AGENT,
          provider: "claude-subscription",
          accountId: null,
          planType: null,
          accessTokenExpiresAt: "2099-01-01T00:00:00Z",
          lastRefresh: null,
          enabled: true,
          pendingSave: false,
        },
      ]),
    });
    render(
      <SetupScreen
        shell={shell}
        initialStep="inference"
        purpose="add-backend"
        provider="anthropic"
        agentDid={AGENT}
        onCancel={vi.fn()}
        onDone={vi.fn()}
      />,
    );
    expect(await screen.findByText("Account connected")).toBeVisible();
    expect(api.claudeLogin).not.toHaveBeenCalled();
  });
});

describe("setup save", () => {
  /* #2068: the runtime confirms the rebound default behavior only after it
     reconciles, starts the slot and replicates readiness back. A confirmation
     that lands after the old five-second budget must still complete the first
     save. */
  it("completes on the first click when readiness arrives after five seconds", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let appliedAt: number | null = null;
    const harness = createDesktopUiHarness({ scenario: "empty-fleet" });
    await harness.adapter.initLocalStandardRuntime({
      label: "Forge",
      dangerouslyOverwrite: false,
      reset: false,
    });
    const { api, shell } = setup(
      {
        async applyConfigComponents(request) {
          appliedAt = Date.now();
          return harness.adapter.applyConfigComponents(request);
        },
        async fetchDesktopSnapshot(): Promise<DesktopClientSnapshot> {
          const snapshot = await harness.adapter.fetchDesktopSnapshot();
          if (appliedAt === null || Date.now() - appliedAt >= 6_000) return snapshot;
          return {
            ...snapshot,
            client: snapshot.client && {
              ...snapshot.client,
              deployments: snapshot.client.deployments.map((deployment) => ({
                ...deployment,
                behaviorReadiness: {
                  ...deployment.behaviorReadiness,
                  behaviors: deployment.behaviorReadiness.behaviors.map((behavior) => ({
                    state: "unavailable" as const,
                    behaviorId: behavior.behaviorId,
                    reason: "backend_disabled" as const,
                  })),
                },
              })),
            },
          };
        },
      },
      harness,
    );
    const onDone = vi.fn();
    render(
      <SetupScreen
        shell={shell}
        initialStep="inference"
        agentDid={AGENT}
        onDone={onDone}
      />,
    );
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    await user.click(await screen.findByTestId("setup-provider-local"));
    await user.click(screen.getByRole("button", { name: "Connect and find models" }));
    await user.click(
      await screen.findByRole("option", { name: "GLM-5.3-Flash-NVFP4" }),
    );
    await user.click(screen.getByTestId("setup-save-inference"));

    await vi.advanceTimersByTimeAsync(7_000);
    await waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
    expect(api.applyConfigComponents).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });
});
