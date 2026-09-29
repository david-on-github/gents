/* The answer surface for an `ask` item whose payload is a typed question:
   one button per option (a click sends it), or toggles plus Send when the
   question takes several, and an "Other" field when it accepts free text.
   Sending is the item's ordinary reply request; the bridge renders its
   content from the question. */
import { Check } from "lucide-react";
import type {
  MailboxItemView,
  MailboxQuestion,
  MailboxQuestionAnswer,
} from "@source-inc/gents-desktop-client";
import { Button } from "@gents/ui/components/button";
import { Input } from "@gents/ui/components/input";
import { toast } from "sonner";
import { useState } from "react";

/* a payload the runtime validated when it was filed; anything else keeps
   the generic reading view */
export function parseQuestion(item: MailboxItemView): MailboxQuestion | null {
  if (item.kind !== "ask" || item.action !== "start_request" || !item.payload) {
    return null;
  }
  try {
    const value = JSON.parse(item.payload) as Partial<MailboxQuestion> | null;
    if (
      !value ||
      value.version !== 1 ||
      typeof value.prompt !== "string" ||
      !Array.isArray(value.options) ||
      value.options.length < 2 ||
      !value.options.every(
        (option) => typeof option?.id === "string" && typeof option?.label === "string",
      )
    ) {
      return null;
    }
    return {
      version: value.version,
      prompt: value.prompt,
      options: value.options,
      multi_select: value.multi_select === true,
      allow_free_text: value.allow_free_text === true,
    };
  } catch {
    return null;
  }
}

export function QuestionAnswer({
  question,
  onAnswer,
}: {
  question: MailboxQuestion;
  onAnswer: (answer: MailboxQuestionAnswer) => Promise<void>;
}) {
  const [selected, setSelected] = useState<string[]>([]);
  const [other, setOther] = useState("");
  const [sending, setSending] = useState(false);
  const send = async (answer: MailboxQuestionAnswer) => {
    setSending(true);
    try {
      await onAnswer(answer);
    } catch (error) {
      toast(`Couldn't send the answer: ${String(error)}`);
    } finally {
      setSending(false);
    }
  };
  const note = other.trim() || null;
  /* a single choice sends on click unless a note is being written with it */
  const deferred = question.multi_select || (question.allow_free_text && note !== null);
  const choose = (id: string) => {
    if (!deferred) {
      void send({ option_ids: [id], free_text: null });
      return;
    }
    setSelected((current) =>
      current.includes(id)
        ? current.filter((value) => value !== id)
        : question.multi_select
          ? [...current, id]
          : [id],
    );
  };
  const canSend = selected.length > 0 || note !== null;
  return (
    <div className="mt-3 max-w-prose" data-testid="mailbox-question">
      <p className="text-sm text-foreground">{question.prompt}</p>
      <div
        className="mt-2 flex flex-wrap gap-2"
        role="group"
        aria-label={question.prompt}
      >
        {question.options.map((option) => {
          const on = selected.includes(option.id);
          return (
            <Button
              key={option.id}
              size="sm"
              variant={on ? "brand" : "raised"}
              aria-pressed={deferred ? on : undefined}
              title={option.description ?? undefined}
              disabled={sending}
              onClick={() => choose(option.id)}
            >
              {deferred && on && <Check />}
              {option.label}
            </Button>
          );
        })}
      </div>
      {question.options.some((option) => option.description) && (
        <dl className="mt-2 grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
          {question.options
            .filter((option) => option.description)
            .map((option) => (
              <div key={option.id} className="contents">
                <dt className="text-foreground">{option.label}</dt>
                <dd>{option.description}</dd>
              </div>
            ))}
        </dl>
      )}
      {(question.allow_free_text || question.multi_select) && (
        <form
          className="mt-2 flex items-center gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (canSend) void send({ option_ids: selected, free_text: note });
          }}
        >
          {question.allow_free_text && (
            <Input
              aria-label="Other"
              placeholder="Other…"
              value={other}
              disabled={sending}
              onChange={(event) => setOther(event.target.value)}
              className="h-8 max-w-xs text-sm"
            />
          )}
          <Button
            type="submit"
            size="sm"
            variant="brand"
            disabled={!canSend || sending}
          >
            Send
          </Button>
        </form>
      )}
    </div>
  );
}
