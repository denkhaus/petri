#!/usr/bin/env python3
"""A scripted Agent Client Protocol agent for the Fabro oracle harness.

Fabro's `agent` handler launches `acp.command` and drives it over ACP on
stdin and stdout. This agent plays one node of one oracle case: its n-th
prompt takes the n-th entry of the node's `calls` script (the last entry
repeats), or the node's single script when there are no `calls`.

For a script that succeeds, is skipped, or fails deterministically, the
agent answers the prompt with a text whose last JSON object is Fabro's
routing directive (`outcome`, `failure_reason`, `preferred_next_label`,
`suggested_next_ids`, `context_updates`). Fabro reads that directive from
the response text the same way it reads it from a model.

For a script whose `failure_class` is `retry_requested`, the agent exits
before answering. Fabro turns the dead process into a retryable handler
error and retries the stage when attempts remain. This is the closest public
behavior to a handler that asks for a retry; see CONTRACT.md for the one
difference (no `allow_partial` promotion on exhaustion, which the harness
covers with a human gate timeout instead).

Usage: scripted_acp_agent.py <case.json> <state dir> <node id>
The state dir counts prompts per node across the agent's separate processes;
the node id comes last so the harness can append it to a shared prefix.
"""
import json
import os
import sys


def load_script(case_path, node):
    with open(case_path, encoding="utf-8") as f:
        case = json.load(f)
    return case.get("scripts", {}).get(node, {})


def script_for_call(script, call):
    calls = script.get("calls") or []
    if calls:
        return calls[min(call, len(calls) - 1)]
    return script


def next_call(state_dir, node):
    os.makedirs(state_dir, exist_ok=True)
    path = os.path.join(state_dir, f"{node}.calls")
    count = 0
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            count = int(f.read().strip() or "0")
    with open(path, "w", encoding="utf-8") as f:
        f.write(str(count + 1))
    return count


def directive(script):
    out = {}
    outcome = script.get("outcome")
    if outcome:
        out["outcome"] = outcome
    if script.get("failure_reason"):
        out["failure_reason"] = script["failure_reason"]
    if script.get("preferred_label"):
        out["preferred_next_label"] = script["preferred_label"]
    if script.get("suggested_next_ids"):
        out["suggested_next_ids"] = script["suggested_next_ids"]
    if script.get("context_updates"):
        out["context_updates"] = script["context_updates"]
    return out


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def respond(message, result):
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})


def main():
    case_path, state_dir, node = sys.argv[1:4]
    session_id = "sess-1"
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        if method == "initialize":
            respond(message, {"protocolVersion": 1, "agentCapabilities": {}})
        elif method == "session/new":
            respond(message, {"sessionId": session_id})
        elif method == "session/prompt":
            script = script_for_call(load_script(case_path, node), next_call(state_dir, node))
            if script.get("failure_class") == "retry_requested":
                print(f"scripted agent for {node}: exiting to request a retry", file=sys.stderr, flush=True)
                sys.exit(3)
            fields = directive(script)
            text = f"[Scripted] {node}"
            if fields:
                text += "\n" + json.dumps(fields)
            send({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": text},
                    },
                },
            })
            respond(message, {"stopReason": "end_turn"})
            break
        elif method == "session/cancel":
            sys.exit(0)
        else:
            send({
                "jsonrpc": "2.0",
                "id": message.get("id"),
                "error": {"code": -32601, "message": "method not found"},
            })


if __name__ == "__main__":
    main()
