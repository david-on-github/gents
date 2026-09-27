/* What a person can do to a running background row from the session that
   made the call: kill it. A process row stops its process; a subagent row's
   call is a started or messaged session, and the runtime owner interrupts
   the one request that call caused. Nothing else stops with it, and nothing
   this session is doing stops. Telling a subagent something is sending its
   session a message, so it is done there; opening it is the row's arrow. */
import { createContext, useContext } from "react";
import { Square } from "lucide-react";
import type { RenderedToolCallView } from "@source-inc/gents-desktop-client";
import { Button } from "@gents/ui/components/button";
import { Hint } from "./Hint";

export type WorkerActions = {
  kill: (tool: RenderedToolCallView) => void;
};

export const WorkerActionsContext = createContext<WorkerActions | null>(null);

export function WorkerStop({
  name,
  tool,
}: {
  name: string;
  tool: RenderedToolCallView;
}) {
  const actions = useContext(WorkerActionsContext);
  /* the row is killable while the background row itself runs; once it
     settles the row keeps its arrow alone */
  if (!actions || tool.statusKind !== "running" || tool.awaitMode !== "background")
    return null;
  return (
    <Hint label={`Stop ${name}`}>
      <Button
        variant="quiet"
        size="icon-xs"
        aria-label={`Stop ${name}`}
        onClick={() => actions.kill(tool)}
      >
        <Square className="size-3.5 fill-current" />
      </Button>
    </Hint>
  );
}
