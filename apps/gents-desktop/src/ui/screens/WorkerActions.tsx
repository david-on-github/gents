/* What a person can do to running work from the session whose call started
   it: stop it. A native process row is killed through the runtime that owns
   the process. A session-message row stops by interrupting the one request
   its call caused, the modeled single-request interrupt; the row settles
   when that request reaches a terminal state. Nothing else stops with it,
   and nothing this session is doing stops. Telling a subagent something is
   sending its session a message, so it is done there; opening it is the
   row's arrow. */
import { createContext, useContext } from "react";
import { Square } from "lucide-react";
import type {
  CausedRequestView,
  RenderedToolCallView,
} from "@source-inc/gents-desktop-client";
import { Button } from "@gents/ui/components/button";
import { Hint } from "./Hint";
import { isLive } from "@/lib/live";

export type WorkerActions = {
  /* kill a native background process row */
  kill: (tool: RenderedToolCallView) => void;
  /* interrupt the request a session-message row's call caused */
  interrupt: (request: CausedRequestView) => void;
};

export const WorkerActionsContext = createContext<WorkerActions | null>(null);

function StopButton({ name, onStop }: { name: string; onStop: () => void }) {
  return (
    <Hint label={`Stop ${name}`}>
      <Button
        variant="quiet"
        size="icon-xs"
        aria-label={`Stop ${name}`}
        onClick={onStop}
      >
        <Square className="size-3.5 fill-current" />
      </Button>
    </Hint>
  );
}

/* Stop on a native process row, while that background row runs. */
export function ProcessStop({
  name,
  tool,
}: {
  name: string;
  tool: RenderedToolCallView;
}) {
  const actions = useContext(WorkerActionsContext);
  if (!actions || tool.statusKind !== "running" || tool.awaitMode !== "background")
    return null;
  return <StopButton name={name} onStop={() => actions.kill(tool)} />;
}

/* Stop on a session-message row, while the request its call caused runs. */
export function RequestStop({
  name,
  request,
}: {
  name: string;
  request: CausedRequestView | null;
}) {
  const actions = useContext(WorkerActionsContext);
  if (!actions || !request || !isLive(request.lifecycleState)) return null;
  return <StopButton name={name} onStop={() => actions.interrupt(request)} />;
}
