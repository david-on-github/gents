/* Mailbox: what the agent filed for a person, after the Figma "Mailbox"
   section and the desktop's vocabulary. An item is a stamped envelope
   the agent wrote with file_mailbox_item: a kind (ask, gate, finished,
   failed, flag), an action it wants (ack: just read it; start_request:
   open a conversation on it; write_document: a document is expected),
   its source, and a summary. A rail down the left carries one glyph per
   kind; each card offers the action the desktop offers for it (Open
   source for ack, Open compose otherwise) and Dismiss. Held tool calls
   are not mailbox items; they live in the session. */
import {
  ArrowLeft,
  ArrowRight,
  CircleCheck,
  CircleHelp,
  CircleX,
  Flag,
  Inbox,
  OctagonPause,
  Plus,
  X,
} from "lucide-react";
import type { MailboxItemView } from "@source-inc/gents-desktop-client";
import { Badge } from "@gents/ui/components/badge";
import { Button } from "@gents/ui/components/button";
import { ScrollArea } from "@gents/ui/components/scroll-area";
import { cn } from "@gents/ui/lib/utils";
import type { Shell } from "@/hooks/useShell";
import { href, navigate } from "@/lib/router";
import { toast } from "sonner";
import { useState, type ComponentProps } from "react";
import { BehaviorAvatar } from "./parts";
import { BehaviorHoverCard } from "./HoverCards";
import { behaviorName } from "./behavior";
import { Markdown } from "./Markdown";
import { parseQuestion, QuestionAnswer } from "./MailboxQuestion";
import { when } from "./time";

type BadgeVariant = ComponentProps<typeof Badge>["variant"];

/* the kind glyph on the rail, its word, and its badge */
const KIND: Record<
  string,
  { icon: typeof CircleHelp; label: string; tone?: string; badge: BadgeVariant }
> = {
  ask: { icon: CircleHelp, label: "Question", badge: "purple" },
  gate: { icon: OctagonPause, label: "Gate", badge: "yellow" },
  finished: { icon: CircleCheck, label: "Finished", badge: "success" },
  failed: {
    icon: CircleX,
    label: "Failed",
    tone: "text-destructive",
    badge: "destructive",
  },
  flag: { icon: Flag, label: "Flag", badge: "secondary" },
};

const STATUS: Record<string, { label: string; badge: BadgeVariant }> = {
  open: { label: "Open", badge: "outline" },
  acted: { label: "Acted on", badge: "secondary" },
  dismissed: { label: "Dismissed", badge: "secondary" },
  expired: { label: "Expired", badge: "destructive" },
};

/* a body longer than this folds behind "Show more"; counted rather than
   measured so the fold does not depend on layout having happened */
const FOLD_CHARS = 600;
const FOLD_LINES = 10;

export function MailboxScreen({ shell }: { shell: Shell }) {
  const deployment = shell.selectedDeployment;
  const items = (deployment?.mailboxItems ?? []).filter((m) => m.status === "open");
  return (
    <ScrollArea className="h-full" data-testid="mailbox-screen">
      <div className="mx-auto max-w-page px-6 py-6">
        <a
          href={href({ name: "sessions" })}
          className="inline-flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
        >
          <ArrowLeft className="size-3" /> Back
        </a>
        {items.length > 0 && (
          <h1 className="mt-2 font-heading text-lg font-medium text-heading">
            Needs your attention
          </h1>
        )}
        {items.length > 0 && (
          <ol className="relative mt-5 grid grid-cols-[minmax(0,1fr)] gap-5 pl-10">
            <span
              aria-hidden="true"
              className="absolute top-3 bottom-3 left-[11px] w-px bg-border"
            />
            {items.map((m) => (
              <Item
                key={m.itemId}
                item={m}
                shell={shell}
                behavior={behaviorName(m.targetBehaviorId, deployment)}
              />
            ))}
          </ol>
        )}
        {items.length === 0 && (
          <div className="grid min-h-[60vh] place-items-center animate-in fade-in-0 slide-in-from-bottom-2 duration-300 ease-out fill-mode-both motion-reduce:animate-none">
            <div className="text-center">
              <Inbox className="mx-auto size-6 text-muted-foreground" />
              <p className="mt-3 font-heading text-lg font-medium text-heading">
                Nothing needs your attention
              </p>
              <p className="mx-auto mt-2 max-w-sm text-sm text-muted-foreground">
                When an agent has a question, needs your approval, finishes or fails
                work, or flags something for you, it shows up here. To give an agent new
                work, start a session.
              </p>
              <Button
                variant="brand"
                className="mt-5"
                nativeButton={false}
                render={<a href={href({ name: "session", sessionId: null })} />}
              >
                <Plus /> Start a session
              </Button>
            </div>
          </div>
        )}
      </div>
    </ScrollArea>
  );
}

function Item({
  item: m,
  shell,
  behavior,
}: {
  item: MailboxItemView;
  shell: Shell;
  behavior: string;
}) {
  const kind = KIND[m.kind] ?? {
    icon: CircleHelp,
    label: m.kind,
    badge: "secondary" as BadgeVariant,
  };
  const status = STATUS[m.status] ?? {
    label: m.status,
    badge: "outline" as BadgeVariant,
  };
  const session = m.sessionId
    ? shell.selectedDeployment?.sessions.find((s) => s.sessionId === m.sessionId)
    : undefined;
  /* a kind with its own answer surface renders it; any other item keeps
     the generic reading view */
  const question = parseQuestion(m);
  const body = [m.summary, m.payload && !question ? payloadMarkdown(m.payload) : null]
    .filter((part): part is string => Boolean(part?.trim()))
    .join("\n\n");
  const foldable = body.length > FOLD_CHARS || body.split("\n").length > FOLD_LINES;
  const [expanded, setExpanded] = useState(false);
  const Icon = kind.icon;
  /* ack: there is nothing to do but read it, so the arrow opens its source;
     anything else opens a conversation on it (the desktop's "Open compose") */
  const acknowledge = m.action === "ack";
  const open = async () => {
    if (acknowledge) {
      if (m.sessionId) navigate({ name: "session", sessionId: m.sessionId });
      return;
    }
    try {
      const item = await shell.openMailboxItem(m.itemId);
      if (!item) return;
      navigate(
        item.sessionId
          ? { name: "session", sessionId: item.sessionId }
          : { name: "session", sessionId: null },
      );
    } catch (e) {
      toast(`Couldn't open: ${String(e)}`);
    }
  };
  const [openedAt] = useState(() => Date.now());
  const deadline = m.deadlineAt ? Date.parse(m.deadlineAt) : null;
  const overdue = deadline !== null && deadline < openedAt;
  const due =
    deadline === null
      ? null
      : overdue
        ? "overdue"
        : `due in ${span(deadline - openedAt)}`;
  const openable = Boolean(m.sessionId) || !acknowledge;
  const age = when(m.createdAt);
  const created = Number.isNaN(Date.parse(m.createdAt))
    ? undefined
    : new Date(m.createdAt).toLocaleString();
  const time = (className: string) => (
    <time dateTime={m.createdAt} title={created} className={className}>
      {age}
    </time>
  );
  return (
    <li className="relative">
      <span
        className={cn(
          "absolute top-3 -left-10 grid size-6 place-items-center rounded-full bg-background text-muted-foreground",
          kind.tone,
        )}
        title={kind.label}
      >
        <Icon className="size-4" />
      </span>
      <article className="rounded-2xl border border-border/60 bg-raised px-4 pt-3 pb-5">
        <div
          className={cn(
            "grid items-start gap-x-3 max-md:gap-y-2",
            openable
              ? "grid-cols-[auto_minmax(0,1fr)_auto]"
              : "grid-cols-[auto_minmax(0,1fr)]",
          )}
        >
          <div className="col-start-1 row-start-1 max-md:self-center md:mt-0.5">
            <BehaviorHoverCard
              deployment={shell.selectedDeployment}
              behaviorId={m.targetBehaviorId}
            >
              <BehaviorAvatar
                name={behavior}
                behaviorId={m.targetBehaviorId}
                className="cursor-default"
              />
            </BehaviorHoverCard>
          </div>
          {time(
            "col-start-2 row-start-1 self-center justify-self-end text-xs text-muted-foreground md:hidden",
          )}
          <div className="min-w-0 max-md:col-span-full max-md:row-start-2 md:col-start-2 md:row-start-1">
            <div className="flex items-baseline gap-3">
              <h2 className="min-w-0 flex-1 font-heading text-sm font-medium text-pretty wrap-break-word text-heading">
                {m.title}
              </h2>
              {time("shrink-0 text-xs text-muted-foreground max-md:hidden")}
            </div>
            <div
              className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground"
              data-testid="mailbox-item-meta"
            >
              <Badge variant={kind.badge}>{kind.label}</Badge>
              <Badge variant={status.badge}>{status.label}</Badge>
              <span>
                from <span className="text-foreground">{behavior}</span>
              </span>
              {m.sessionId && (
                <a
                  href={href({ name: "session", sessionId: m.sessionId })}
                  className="max-w-64 truncate underline-offset-2 hover:text-foreground hover:underline"
                >
                  in {session?.title?.trim() || "session"}
                </a>
              )}
              {due && (
                <span
                  className={cn("whitespace-nowrap", overdue && "text-destructive")}
                >
                  {due}
                </span>
              )}
            </div>
            {body && (
              <div className="mt-2 max-w-prose">
                <div
                  data-testid="mailbox-item-body"
                  className={cn(
                    "prose-app relative",
                    foldable && !expanded && "max-h-48 overflow-hidden",
                  )}
                >
                  <Markdown breaks>{body}</Markdown>
                  {foldable && !expanded && (
                    <div className="pointer-events-none absolute inset-x-0 bottom-0 h-12 bg-gradient-to-t from-raised to-transparent" />
                  )}
                </div>
                {foldable && (
                  <Button
                    variant="quiet"
                    size="xs"
                    className="mt-1 -ml-2"
                    aria-expanded={expanded}
                    onClick={() => setExpanded((v) => !v)}
                  >
                    {expanded ? "Show less" : "Show more"}
                  </Button>
                )}
              </div>
            )}
            {question && (
              <QuestionAnswer
                question={question}
                onAnswer={(answer) => shell.answerMailboxQuestion(m, answer)}
              />
            )}
            {m.action === "write_document" && m.expectedCollection && (
              <p className="mt-2 text-xs text-muted-foreground">
                Answer with a{" "}
                <span className="font-mono text-foreground">
                  {m.expectedCollection}
                </span>{" "}
                document
              </p>
            )}
          </div>
          {openable && (
            <Button
              size="icon-sm"
              variant="ghost"
              className="col-start-3 row-start-1 -mr-1 self-center"
              aria-label={
                acknowledge
                  ? "Open source"
                  : m.sessionId
                    ? "Open compose"
                    : "Start a conversation"
              }
              title={acknowledge ? "Open source" : "Open compose"}
              onClick={() => void open()}
            >
              <ArrowRight />
            </Button>
          )}
        </div>
      </article>
      <Button
        size="sm"
        variant="raised"
        className="-mt-3.5 ml-4 flex h-7 w-fit rounded-full px-3 text-xs"
        onClick={() => shell.dismissMailboxItem(m.itemId)}
      >
        <X className="size-3" /> Dismiss
      </Button>
    </li>
  );
}

/* a payload that is JSON reads as a fenced block; anything else is the
   agent's own markdown */
const payloadMarkdown = (value: string) => {
  let json: string;
  try {
    const parsed: unknown = JSON.parse(value);
    if (typeof parsed === "string") return parsed;
    json = JSON.stringify(parsed, null, 2);
  } catch {
    return value;
  }
  let fence = "```";
  while (json.includes(fence)) fence += "`";
  return `${fence}json\n${json}\n${fence}`;
};

/* a span ahead, as a person would say it */
const span = (ms: number) => {
  const mins = Math.max(1, Math.round(ms / 60_000));
  if (mins < 60) return `${mins}m`;
  if (mins < 1_440) return `${Math.round(mins / 60)}h`;
  return `${Math.round(mins / 1_440)}d`;
};
