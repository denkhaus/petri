#!/usr/bin/env python3
"""Run the shared oracle cases through the pinned Fabro binary.

This is Petri's testing and parity harness. It never links Fabro. It drives
the `fabro` executable that scripts/oracle-regenerate.sh built from the
pinned checkout, through public interfaces only:

* `fabro server start` on a loopback port with an isolated storage directory,
  dev-token auth, a `local` environment, and a fake provider secret so run
  creation can select a default model.
* `fabro run --detach --environment local --provider openai` for each case,
  in a fresh git repository that holds the case's workflow.
* `GET /api/v1/runs/{id}/state` and `GET /api/v1/runs/{id}/questions` plus
  `POST /api/v1/runs/{id}/questions/{qid}/answer` to answer human gates.
* `fabro events <run>` to read the stage path, loop restarts, the final
  context, and the run outcome.

How a case script becomes Fabro behavior (see CONTRACT.md):

* An agent or prompt node gets `backend="acp"` and an `acp.command` that
  launches scripted_acp_agent.py. The agent answers with a text whose last
  JSON object is Fabro's routing directive.
* A node whose script asks for `failure_class=retry_requested` becomes a
  human gate with a short timeout. A timed-out gate with no default choice
  is the one public handler result that carries `retry_requested`, so
  `max_retries` and `allow_partial` apply the way the case intends. A call
  that is not a retry request is answered through the questions API.
* A human node scripted to fail is left unanswered so it times out.
* A human node scripted to succeed is answered with the option that matches
  its `preferred_label`, else its first `suggested_next_ids` target, else
  the first choice.

Usage: oracle_harness.py --fabro BIN --commit SHA --cases DIR --expected DIR
                         [--case NAME]... [--work DIR] [--keep]
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

DEV_TOKEN = "fabro_dev_abababababababababababababababababababababababababababababababab"
SESSION_SECRET = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
HUMAN_TIMEOUT = "2s"
RUN_DEADLINE_SECONDS = 240
INTERNAL_PREFIXES = ("internal.", "graph.", "response.", "thread.", "current", "human.gate.")
INTERNAL_KEYS = {
    "outcome", "failure_class", "failure_signature", "preferred_label", "last_stage",
    "last_response", "command.output", "parallel.results", "parallel.branch_count",
}
NODE_LINE = re.compile(r'^(\s*)([A-Za-z_][A-Za-z0-9_]*)\s*\[(.*)\]\s*$')
EDGE = re.compile(r'([A-Za-z_][A-Za-z0-9_]*)\s*->\s*([A-Za-z_][A-Za-z0-9_]*)(?:\s*\[([^\]]*)\])?')
ATTR = re.compile(r'([A-Za-z_][A-Za-z0-9_.]*)\s*=\s*(?:"((?:[^"\\]|\\.)*)"|([^,\]\s]+))')


def log(message):
    print(message, file=sys.stderr, flush=True)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def attrs_of(text):
    return {m.group(1): m.group(2) if m.group(2) is not None else m.group(3) for m in ATTR.finditer(text)}


def is_agent_node(attrs):
    shape = attrs.get("shape")
    if shape in ("Mdiamond", "Msquare", "diamond", "hexagon", "component", "tripleoctagon", "insulator", "house", "parallelogram"):
        return False
    if "script" in attrs:
        return False
    return "prompt" in attrs or shape in ("box", "tab", None)


def is_human_node(attrs):
    return attrs.get("shape") == "hexagon"


def needs_gate(script):
    """Does any call of this script ask for an outcome-level retry request?"""
    calls = script.get("calls") or [script]
    return any(c.get("failure_class") == "retry_requested" for c in calls)


def rewrite_workflow(case, agent_command):
    """Bind each scripted node to its public realization."""
    lines = case["workflow"].splitlines()
    scripts = case.get("scripts", {})
    kinds = {}
    out = []
    for line in lines:
        m = NODE_LINE.match(line)
        if not m or "->" in line:
            out.append(line)
            continue
        indent, node, body = m.groups()
        if node in ("graph", "node", "edge"):
            out.append(line)
            continue
        attrs = attrs_of(body)
        script = scripts.get(node, {})
        if is_human_node(attrs):
            kinds[node] = "human"
            if script.get("outcome") == "failed" or needs_gate(script):
                body += f', timeout="{HUMAN_TIMEOUT}"'
            out.append(f"{indent}{node} [{body}]")
        elif is_agent_node(attrs):
            if needs_gate(script):
                kinds[node] = "human"
                body = re.sub(r'\bprompt\s*=\s*"(?:[^"\\]|\\.)*"\s*,?\s*', "", body).strip().rstrip(",")
                extra = f'shape=hexagon, label="{node}", timeout="{HUMAN_TIMEOUT}"'
                body = f"{extra}, {body}" if body else extra
            else:
                kinds[node] = "agent"
                body += f', backend="acp", acp.command="{agent_command} {node}"'
            out.append(f"{indent}{node} [{body}]")
        else:
            kinds[node] = attrs.get("shape", "other")
            out.append(line)
    return "\n".join(out) + "\n", kinds


def parse_edges(workflow):
    edges = []
    for m in EDGE.finditer(workflow):
        edges.append((m.group(1), m.group(2), attrs_of(m.group(3) or "")))
    return edges


def normalize_label(label):
    text = label.strip()
    m = re.match(r'^\[([^\]]+)\]\s*(.*)$', text)
    if m:
        text = m.group(2) or m.group(1)
    else:
        m = re.match(r'^([A-Za-z0-9]{1,3})\)\s*(.*)$', text)
        if m:
            text = m.group(2) or m.group(1)
        else:
            m = re.match(r'^([A-Za-z0-9]{1,3})\s-\s(.*)$', text)
            if m:
                text = m.group(2) or m.group(1)
    return re.sub(r"\s+", " ", text).strip().lower()


class Server:
    def __init__(self, fabro, root):
        self.fabro = fabro
        self.root = root
        self.home = os.path.join(root, "home")
        self.storage = os.path.join(root, "storage")
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.process = None

    def env(self):
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": self.home,
            "FABRO_HOME": os.path.join(self.home, ".fabro"),
            "FABRO_SERVER": self.url,
            "FABRO_DEV_TOKEN": DEV_TOKEN,
            "SESSION_SECRET": SESSION_SECRET,
            "FABRO_NO_UPGRADE_CHECK": "true",
            "FABRO_HTTP_PROXY_POLICY": "disabled",
            "FABRO_TELEMETRY": "off",
            "FABRO_SUPPRESS_OPEN_BROWSER": "1",
            "FABRO_TEST_IN_MEMORY_STORE": "1",
            "NO_COLOR": "1",
            "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
        }
        return env

    def start(self):
        os.makedirs(os.path.join(self.home, ".fabro"), exist_ok=True)
        os.makedirs(self.storage, exist_ok=True)
        config = os.path.join(self.home, ".fabro", "settings.toml")
        with open(config, "w", encoding="utf-8") as f:
            f.write(
                "_version = 1\n\n[server.storage]\nroot = \"%s\"\n\n[server.auth]\nmethods = [\"dev-token\"]\n\n"
                "[cli.target]\ntype = \"http\"\nurl = \"%s\"\n\n[environments.local]\nprovider = \"local\"\n"
                % (self.storage, self.url)
            )
        log_path = os.path.join(self.root, "server.log")
        self.log = open(log_path, "w", encoding="utf-8")
        self.process = subprocess.Popen(
            [self.fabro, "server", "start", "--foreground", "--no-web", "--storage-dir", self.storage,
             "--bind", f"127.0.0.1:{self.port}", "--config", config],
            env=self.env(), stdout=self.log, stderr=subprocess.STDOUT,
        )
        deadline = time.time() + 60
        while time.time() < deadline:
            if self.process.poll() is not None:
                raise SystemExit(f"fabro server exited early; see {log_path}")
            try:
                self.api("GET", "/api/v1/health")
                break
            except Exception:
                time.sleep(0.25)
        else:
            raise SystemExit(f"fabro server did not become healthy; see {log_path}")
        self.cli("auth", "login", "--dev-token", DEV_TOKEN)
        # Run creation needs one ready provider to pick a default model. The
        # key is fake; no model is ever called (agent nodes use ACP).
        self.cli("secret", "set", "OPENAI_API_KEY", "test")

    def cli(self, *args, cwd=None, check=True):
        result = subprocess.run([self.fabro, *args], env=self.env(), cwd=cwd, capture_output=True, text=True)
        if check and result.returncode != 0:
            raise SystemExit(f"fabro {' '.join(args)} failed:\n{result.stdout}\n{result.stderr}")
        return result

    def api(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.url + path, data=data, method=method)
        request.add_header("Authorization", f"Bearer {DEV_TOKEN}")
        if data is not None:
            request.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(request, timeout=30) as response:
            text = response.read().decode()
            return json.loads(text) if text else None

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
        if getattr(self, "log", None):
            self.log.close()


def answer_for(script, call_index, options, edges, node):
    calls = script.get("calls") or []
    call = calls[min(call_index, len(calls) - 1)] if calls else script
    if call.get("failure_class") == "retry_requested" or call.get("outcome") == "failed":
        return None
    if call.get("preferred_label"):
        wanted = normalize_label(call["preferred_label"])
        for option in options:
            if normalize_label(option["label"]) == wanted:
                return option["key"]
    for target in call.get("suggested_next_ids", []):
        for edge_from, edge_to, edge_attrs in edges:
            if edge_from == node and edge_to == target:
                label = edge_attrs.get("label") or edge_to
                for option in options:
                    if option["label"] == label:
                        return option["key"]
    return options[0]["key"] if options else None


def run_case(server, case, harness_dir, work, keep):
    name = case["name"]
    case_dir = os.path.join(work, name)
    shutil.rmtree(case_dir, ignore_errors=True)
    repo = os.path.join(case_dir, "repo")
    state = os.path.join(case_dir, "state")
    os.makedirs(repo)
    os.makedirs(state)
    case_path = os.path.join(case_dir, "case.json")
    with open(case_path, "w", encoding="utf-8") as f:
        json.dump(case, f)
    agent = os.path.join(harness_dir, "scripted_acp_agent.py")
    # scripted_acp_agent.py takes <case> <state dir> <node>; the node id is
    # appended per node by rewrite_workflow.
    workflow, kinds = rewrite_workflow(case, f"python3 {agent} {case_path} {state}")
    with open(os.path.join(repo, "case.fabro"), "w", encoding="utf-8") as f:
        f.write(workflow)
    with open(os.path.join(case_dir, "realized.fabro"), "w", encoding="utf-8") as f:
        f.write(workflow)
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=repo, check=True)
    subprocess.run(["git", "-c", "user.email=oracle@petri", "-c", "user.name=oracle", "commit", "-q", "--allow-empty", "-m", "case"], cwd=repo, check=True)
    edges = parse_edges(case["workflow"])

    # Fabro validates before it creates a run. A case Fabro refuses is a
    # result too: the fixture records the rejection instead of a path.
    validated = subprocess.run(
        [server.fabro, "validate", "--json", "case.fabro"],
        env=server.env(), cwd=repo, capture_output=True, text=True,
    )
    if validated.returncode != 0:
        try:
            report = json.loads(validated.stdout)
            diagnostics = [
                {k: d.get(k) for k in ("rule", "severity", "message", "node_id", "fix")}
                for d in report.get("diagnostics", [])
            ]
        except ValueError:
            diagnostics = validated.stderr.strip().splitlines()
        log(f"{name}: fabro validate rejected the workflow")
        return {
            "status": "rejected",
            "rejected_by": "fabro validate",
            "diagnostics": diagnostics,
        }, kinds

    result = subprocess.run(
        [server.fabro, "run", "--detach", "--environment", "local", "--provider", "openai", "--json", "case.fabro"],
        env=server.env(), cwd=repo, capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise SystemExit(f"{name}: fabro run failed:\n{result.stdout}\n{result.stderr}")
    run_id = None
    try:
        # `--detach --json` prints one pretty-printed document.
        doc = json.loads(result.stdout)
        run_id = doc.get("run_id") if isinstance(doc, dict) else None
    except ValueError:
        for line in result.stdout.splitlines():
            try:
                doc = json.loads(line.strip())
            except ValueError:
                continue
            if isinstance(doc, dict) and doc.get("run_id"):
                run_id = doc["run_id"]
    if not run_id:
        raise SystemExit(f"{name}: no run id in:\n{result.stdout}")

    # Drive human gates until the run ends. Each question id is decided once:
    # answered, or left to time out. A question left alone is still pending on
    # the next poll and must not be read as a second call of its node.
    decided = set()
    gate_calls = {}
    deadline = time.time() + RUN_DEADLINE_SECONDS
    while time.time() < deadline:
        state_doc = server.api("GET", f"/api/v1/runs/{run_id}/state")
        status = state_doc.get("status")
        kind = status.get("kind") if isinstance(status, dict) else status
        if kind in ("succeeded", "failed", "dead"):
            break
        questions = server.api("GET", f"/api/v1/runs/{run_id}/questions")
        items = questions.get("data", questions) if isinstance(questions, dict) else questions
        for question in items or []:
            qid = question["id"]
            if qid in decided:
                continue
            decided.add(qid)
            node = question.get("stage")
            script = case.get("scripts", {}).get(node, {})
            index = gate_calls.get(node, 0)
            gate_calls[node] = index + 1
            key = answer_for(script, index, question.get("options", []), edges, node)
            if key is None:
                # The scripted failure or retry request: the gate times out.
                continue
            server.api("POST", f"/api/v1/runs/{run_id}/questions/{qid}/answer", {"kind": "selected", "option_key": key})
        time.sleep(0.2)
    else:
        try:
            server.api("POST", f"/api/v1/runs/{run_id}/cancel")
        except Exception:
            pass
        raise SystemExit(f"{name}: run {run_id} did not finish within {RUN_DEADLINE_SECONDS}s")

    events_text = server.cli("events", run_id).stdout
    events = [json.loads(l) for l in events_text.splitlines() if l.strip()]
    with open(os.path.join(case_dir, "events.jsonl"), "w", encoding="utf-8") as f:
        f.write(events_text)
    projection = project(events)
    if not keep:
        shutil.rmtree(repo, ignore_errors=True)
    return projection, kinds


def project(events):
    path = []
    restarts = 0
    completed = False
    failed = False
    context = {}
    for event in events:
        kind = event.get("event")
        props = event.get("properties", {}) or {}
        node = event.get("node_id")
        if kind == "stage.completed" and node:
            path.append({"node": node, "outcome": props.get("status")})
            # `exit` and `start` carry no context snapshot; keep the last one.
            if "context_values" in props:
                context = dict(props["context_values"] or {})
            context.update(props.get("context_updates") or {})
        elif kind == "stage.failed" and node:
            if props.get("will_retry"):
                continue
            path.append({"node": node, "outcome": "failed"})
        elif kind == "checkpoint.completed" and "context_values" in props:
            context = dict(props["context_values"] or {})
        elif kind == "loop.restart":
            restarts += 1
        elif kind == "run.completed":
            completed = True
        elif kind == "run.failed":
            failed = True
    status = "success" if completed and not failed else "failed"
    public = {
        k: v for k, v in sorted(context.items())
        if not k.startswith(INTERNAL_PREFIXES) and k not in INTERNAL_KEYS
    }
    return {"status": status, "path": path, "context": public, "executions": restarts + 1}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fabro", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--cases", required=True)
    parser.add_argument("--expected", required=True)
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--work", default=os.path.join(os.environ.get("TMPDIR", "/tmp"), "petri-fabro-oracle"))
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()

    args.fabro = os.path.abspath(args.fabro)
    args.cases = os.path.abspath(args.cases)
    args.expected = os.path.abspath(args.expected)
    version = subprocess.run([args.fabro, "--version"], capture_output=True, text=True, check=True).stdout.strip()
    if args.commit[:7] not in version:
        raise SystemExit(f"{args.fabro} reports `{version}`, which is not the pinned commit {args.commit}")
    harness_dir = os.path.dirname(os.path.abspath(__file__))
    work = os.path.abspath(args.work)
    if len(work) > 60:
        # Unix socket and path limits inside Fabro; keep the work root short.
        work = os.path.join("/tmp", f"petri-oracle-{os.getpid()}")
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    log(f"fabro: {version}; work: {work}")

    names = sorted(f[:-5] for f in os.listdir(args.cases) if f.endswith(".json"))
    if args.case:
        names = [n for n in names if n in set(args.case)]
    server = Server(args.fabro, os.path.join(work, "server"))
    server.start()
    try:
        os.makedirs(args.expected, exist_ok=True)
        for name in names:
            with open(os.path.join(args.cases, f"{name}.json"), encoding="utf-8") as f:
                case = json.load(f)
            started = time.time()
            fabro, kinds = run_case(server, case, harness_dir, work, args.keep)
            out_path = os.path.join(args.expected, f"{name}.json")
            existing = None
            if os.path.exists(out_path):
                with open(out_path, encoding="utf-8") as f:
                    existing = json.load(f)
            fixture = {
                "schema_version": 2,
                "fabro_commit": args.commit,
                "harness": {
                    "source": "fabro binary through the CLI and server API; see crates/fabro/oracle/harness/",
                    "realization": {node: kind for node, kind in kinds.items() if kind in ("agent", "human")},
                },
                "fabro": fabro,
            }
            if case.get("departure"):
                fixture["departure"] = case["departure"]
                if existing and existing.get("petri") is not None:
                    fixture["petri"] = existing["petri"]
            with open(out_path, "w", encoding="utf-8") as f:
                json.dump(fixture, f, indent=2)
                f.write("\n")
            path = [p["node"] for p in fabro.get("path", [])]
            log(f"{name}: {fabro['status']} path={path} ({time.time() - started:.1f}s)")
    finally:
        server.stop()
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
