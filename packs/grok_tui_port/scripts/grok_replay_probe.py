#!/usr/bin/env python3
"""Read-only replay fidelity check for a completed request on a test server.

Compares the assistant text that `session/load` replays for one request with
the request's canonical assistant messages, and checks that replayed chunks do
not acquire timestamps after the request terminalized. The canonical text comes
from `gents trace project`, which presents each message through the same owner
the runtime uses; this probe never reassembles output segments itself.
"""
import argparse
import json
import subprocess
from datetime import datetime
from typing import Any

from grok_edge_probe import CODEX_PROJECTION_FIXTURE, LeaderClient
from grok_probe_common import graphql_escape, graphql_query, require


def replay_prompt_id(request: dict[str, Any]) -> str:
    """The promptId the shim stamps on a replayed request.

    `session/load` labels a human turn by its request id and a
    runtime-sourced turn (a wake) by `notifications-<request id>`, the ID
    family the stock client hides.
    """
    if request.get("runtime_source_kind"):
        return "notifications-" + request["request_id"]
    return request["request_id"]


def require_completed_request(rows: list[dict[str, Any]], session: str) -> dict[str, Any]:
    require(len(rows) == 1, f"expected exactly one AgentRequest, got {rows}")
    request = rows[0]
    require(request.get("session_id") == session, f"request is not in session {session}: {request}")
    require(request.get("lifecycle_state") == "completed", f"request did not complete: {request}")
    require(request.get("terminalized_at"), f"completed request lacks terminalized_at: {request}")
    return request


def expected_assistant_text(projection: dict[str, Any], request_id: str) -> str:
    """The request's assistant message text in transcript order."""
    items = projection.get("output", {}).get("projection", {}).get("items")
    require(isinstance(items, list), f"trace projection has no items: {projection}")
    return "".join(
        item.get("content", "")
        for item in items
        if item.get("type") == "message"
        and item.get("role") == "assistant"
        and item.get("request_id") == request_id
    )


def timestamp_ms(value: str) -> int:
    return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)


def self_test() -> dict[str, int]:
    require(replay_prompt_id({"request_id": "r1"}) == "r1", "a human turn replays under its request id")
    wake = {"request_id": "r2", "runtime_source_kind": "local_control"}
    require(replay_prompt_id(wake) == "notifications-r2", "a wake replays in the notifications family")
    completed = {
        "request_id": "r1",
        "session_id": "s",
        "lifecycle_state": "completed",
        "terminalized_at": "2026-09-27T00:00:00Z",
    }
    require(require_completed_request([completed], "s") is completed, "completed request rejected")
    rejected = 0
    for rows, session in (
        ([], "s"),
        ([completed, completed], "s"),
        ([completed], "other"),
        ([dict(completed, lifecycle_state="failed")], "s"),
        ([dict(completed, terminalized_at=None)], "s"),
    ):
        try:
            require_completed_request(rows, session)
        except AssertionError:
            rejected += 1
        else:
            raise AssertionError(f"accepted {rows} in {session}")
    envelope = json.loads(CODEX_PROJECTION_FIXTURE.read_text())
    require(
        expected_assistant_text(envelope, envelope["source_request_id"])
        == "I will inspect the workspace.Projection contracts verified.",
        "assistant text drifted from the native projection fixture",
    )
    require(expected_assistant_text(envelope, "other-request") == "", "another request's text leaked")
    require(timestamp_ms("1970-01-01T00:00:01Z") == 1000, "timestamp parse drifted")
    return {"accepted": 4, "rejected": rejected}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="run the offline self-test only")
    for name in ("socket", "graphql", "cwd", "session", "request"):
        parser.add_argument("--" + name)
    parser.add_argument("--gents", default="gents", help="gents binary used for the trace projection")
    args = parser.parse_args()
    if args.self_test:
        print(json.dumps({"self_test": self_test()}, sort_keys=True))
        return
    for name in ("socket", "graphql", "cwd", "session", "request"):
        require(getattr(args, name), f"--{name} is required")
    request_id = graphql_escape(args.request)
    data = graphql_query(
        args.graphql,
        '{AgentRequest(filter:{request_id:{_eq:"' + request_id + '"}}){'
        "request_id session_id runtime_source_kind lifecycle_state terminalized_at}}",
    )
    request = require_completed_request(data.get("AgentRequest") or [], args.session)
    end_ms = timestamp_ms(request["terminalized_at"])
    prompt_id = replay_prompt_id(request)
    projection = json.loads(
        subprocess.run(
            [args.gents, "trace", "project", "--graphql", args.graphql,
             "--request-id", args.request, "--projection", "openai-codex"],
            check=True, capture_output=True, text=True,
        ).stdout
    )
    expected = expected_assistant_text(projection, args.request)
    require(expected, "This probe requires persisted assistant text")
    client = LeaderClient(args.socket, 30, "GLM-5.3-Flash-NVFP4")
    try:
        client.register()
        response, _ = client.request("initialize", {"protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "replay-fidelity-probe", "version": "1"}})
        require("error" not in response, str(response))
        response, events = client.request("session/load", {"sessionId": args.session,
            "cwd": args.cwd, "mcpServers": []})
        require("error" not in response, str(response))
        actual = []
        for event in events:
            params = event.get("params", {})
            meta = params.get("_meta", {})
            update = params.get("update", {})
            if meta.get("promptId") != prompt_id:
                continue
            kind = update.get("sessionUpdate")
            if kind in ("agent_message_chunk", "agent_thought_chunk"):
                require(meta.get("isReplay") is True, str(event))
                require(meta["agentTimestampMs"] <= end_ms, str(event))
            if kind == "agent_message_chunk":
                actual.append(update["content"]["text"])
        actual = "".join(actual)
        require(actual == expected, json.dumps({"expected": expected, "actual": actual}))
        print(json.dumps({"result": "PASS", "request": args.request,
            "assistant_characters": len(actual), "historical_timestamp_ceiling_ms": end_ms}))
    finally:
        client.close()


if __name__ == "__main__":
    main()
