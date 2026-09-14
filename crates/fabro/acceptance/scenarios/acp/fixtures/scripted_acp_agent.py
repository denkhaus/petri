#!/usr/bin/env python3
"""A scripted Agent Client Protocol agent for the black box `acp` family.

A scenario commits this file and a script beside it into the fixture
repository (`.fabro/scripted_acp_agent.py`, `.fabro/acp-script.json`).
Petri checks the repository out into the run workspace, on the host or in
the container, and `acp.command="python3 .fabro/scripted_acp_agent.py"`
starts one process per agent turn with the workspace as its working
directory. Nothing reaches the agent through the environment: everything it
does comes from the script and every trace it leaves is a file under the
workspace, which the scenario's `expect.files` reads back.

Protocol subset (ACP over JSON-RPC lines on stdin and stdout, the subset
Petri's client in `crates/attractor/steps/src/acp.rs` speaks):

* `initialize` is answered with protocol version 1 and no capabilities.
* `session/new` is answered with a fixed session id.
* `session/prompt` runs one scripted call (below); the agent's text goes
  back as `session/update` notifications with `agent_message_chunk`
  content, and the prompt's response carries `stopReason`.
* `session/request_permission` is a request the agent sends; the client's
  answer picks one of the offered options.
* `session/cancel` is a notification the client sends to end a turn.
* Any other method is answered with JSON-RPC `-32601`.

Script format (`.fabro/acp-script.json`, an object of keys):

    {
      "[acp:review]": [ { ...call 1... }, { ...call 2... } ],
      "[acp:wait]":   [ { ... } ]
    }

The key is a text the node's prompt contains. Each `session/prompt` finds
the first key present in the prompt text and takes the next entry of its
list: the n-th prompt for that key runs the n-th entry (the last entry
repeats). Calls are counted in `.fabro/acp-state/<key>.calls`, so a retry,
which starts a new process, sees the next entry. A prompt no key matches
is answered with a fixed text and no directive.

Each call entry is an object; every field is optional and they run in this
order:

    "write":       a file or a list of files to write into the workspace:
                   {"path": ..., "text": ..., "append": false}
    "text":        a string or a list of strings, sent as text chunks
    "exit":        an exit code: the process exits here, before answering
                   the prompt (Petri sees a dead agent and may retry)
    "permission":  send `session/request_permission` for one tool call:
                   {"tool": <title>, "kind": <kind>, "input": {...},
                    "record": <file the chosen option is written to, as
                    the JSON outcome the client answered>,
                    "effect": {"path": ..., "text": ...} written only when
                    the answer allows the call}
    "wait_for_cancel": write a marker file, then wait for `session/cancel`:
                   {"marker": <file written once waiting>,
                    "record": <file written when the cancel arrives>}
                   The prompt is answered with `stopReason: "cancelled"`
                   and the process exits 0.
    "directive":   an object appended as the last text chunk, on its own
                   line: Fabro's routing directive (`outcome`,
                   `failure_reason`, `preferred_next_label`,
                   `suggested_next_ids`, `context_updates`)
    "stop_reason": the prompt response's `stopReason` (default `end_turn`)

Usage: scripted_acp_agent.py [script path] [state directory]
Defaults are `.fabro/acp-script.json` and `.fabro/acp-state`, relative to
the working directory.
"""
import json
import os
import re
import signal
import sys

SESSION_ID = "scripted-session-1"
UNSCRIPTED_TEXT = "[scripted agent] no script entry matches this prompt"


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def respond(message, result):
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})


def text_chunk(text):
    send({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": SESSION_ID,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            },
        },
    })


def write_file(spec):
    path = spec["path"]
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    mode = "a" if spec.get("append") else "w"
    with open(path, mode, encoding="utf-8") as handle:
        handle.write(spec.get("text", ""))


def prompt_text(message):
    parts = message.get("params", {}).get("prompt", [])
    texts = []
    for part in parts:
        if isinstance(part, str):
            texts.append(part)
        elif part.get("type") == "text":
            texts.append(part.get("text", ""))
    return "\n".join(texts)


def load_script(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def state_name(key):
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", key).strip("_") or "key"


def next_call(state_dir, key):
    os.makedirs(state_dir, exist_ok=True)
    path = os.path.join(state_dir, state_name(key) + ".calls")
    count = 0
    if os.path.exists(path):
        with open(path, encoding="utf-8") as handle:
            count = int(handle.read().strip() or "0")
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(str(count + 1))
    return count


def select_call(script, state_dir, text):
    for key, calls in script.items():
        if key in text and calls:
            index = next_call(state_dir, key)
            return calls[min(index, len(calls) - 1)]
    return None


def read_message():
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)


def request_permission(spec):
    request_id = "permission-1"
    send({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": SESSION_ID,
            "toolCall": {
                "toolCallId": "call-1",
                "title": spec.get("tool", "tool"),
                "kind": spec.get("kind", "execute"),
                "rawInput": spec.get("input", {}),
            },
            "options": [
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                {"optionId": "once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "always", "name": "Allow always", "kind": "allow_always"},
            ],
        },
    })
    while True:
        message = read_message()
        if message is None:
            sys.exit(0)
        if message.get("id") == request_id and "method" not in message:
            break
    outcome = message.get("result", {}).get("outcome", {})
    if spec.get("record"):
        write_file({
            "path": spec["record"],
            "text": json.dumps(outcome, sort_keys=True, separators=(",", ":")) + "\n",
        })
    allowed = outcome.get("outcome") == "selected" and outcome.get("optionId") in ("once", "always")
    if allowed and spec.get("effect"):
        write_file(spec["effect"])
    return allowed


def wait_for_cancel(message, spec):
    if spec.get("marker"):
        write_file({"path": spec["marker"], "text": ""})
    while True:
        incoming = read_message()
        if incoming is None:
            sys.exit(0)
        if incoming.get("method") == "session/cancel":
            if spec.get("record"):
                write_file({"path": spec["record"], "text": "session/cancel\n"})
            respond(message, {"stopReason": "cancelled"})
            sys.exit(0)


def run_call(message, call):
    writes = call.get("write", [])
    if isinstance(writes, dict):
        writes = [writes]
    for spec in writes:
        write_file(spec)
    texts = call.get("text", [])
    if isinstance(texts, str):
        texts = [texts]
    for text in texts:
        text_chunk(text)
    if "exit" in call:
        print("scripted agent: exiting before answering", file=sys.stderr, flush=True)
        sys.exit(int(call["exit"]))
    if "permission" in call:
        request_permission(call["permission"])
    if "wait_for_cancel" in call:
        wait_for_cancel(message, call["wait_for_cancel"])
    if "directive" in call:
        text_chunk("\n" + json.dumps(call["directive"]))
    respond(message, {"stopReason": call.get("stop_reason", "end_turn")})


def main():
    script_path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(".fabro", "acp-script.json")
    state_dir = sys.argv[2] if len(sys.argv) > 2 else os.path.join(".fabro", "acp-state")
    signal.signal(signal.SIGTERM, lambda signum, frame: sys.exit(0))
    while True:
        message = read_message()
        if message is None:
            return
        method = message.get("method")
        if method == "initialize":
            respond(message, {"protocolVersion": 1, "agentCapabilities": {}})
        elif method == "session/new":
            respond(message, {"sessionId": SESSION_ID})
        elif method == "session/prompt":
            call = select_call(load_script(script_path), state_dir, prompt_text(message))
            if call is None:
                text_chunk(UNSCRIPTED_TEXT)
                respond(message, {"stopReason": "end_turn"})
            else:
                run_call(message, call)
        elif method == "session/cancel":
            return
        elif "id" in message and method is not None:
            send({
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32601, "message": f"method not found: {method}"},
            })


if __name__ == "__main__":
    main()
