#!/usr/bin/env python3
"""A stand-in for `claude -p --input-format stream-json --output-format stream-json`.

It follows the fleet's headless contract closely enough for end-to-end tests: it prints an
init event with a session id, reads user messages and control requests from stdin, and calls
fleet tools through the node's loopback IPC exactly as the `fleet mcp` shim would (the shim
itself is tested separately).
"""
import base64
import hashlib
import json
import os
import socket
import sys
import time
import uuid


def out(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def arg(name):
    argv = sys.argv
    return argv[argv.index(name) + 1] if name in argv else None


def kit_cli(argv):
    """`claude plugin ...` and `claude mcp ...`, as the node's kit runs them.

    Every call is logged, and its effect is made on the config files the way the real CLI makes
    it (verified against claude 2.1), so the node's next look sees the result. Only ever inside
    CLAUDE_CONFIG_DIR: a test must never touch the developer's own ~/.claude.
    """
    cfg = os.environ.get("CLAUDE_CONFIG_DIR")
    if not cfg:
        sys.stderr.write("refusing: CLAUDE_CONFIG_DIR is not set\n")
        return 2
    with open(os.path.join(cfg, "fake-cli.log"), "a") as f:
        f.write(json.dumps(argv) + "\n")

    def load(path, default):
        try:
            return json.load(open(path))
        except (OSError, ValueError):
            return default

    def save(path, value):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        json.dump(value, open(path, "w"), indent=2)

    claude_json = os.path.join(cfg, ".claude.json")
    plugins = os.path.join(cfg, "plugins")
    installed_path = os.path.join(plugins, "installed_plugins.json")
    known_path = os.path.join(plugins, "known_marketplaces.json")
    rest = [a for a in argv if a not in ("--scope", "user")]
    if rest[:2] == ["mcp", "add-json"]:
        name, config = rest[2], json.loads(rest[3])
        data = load(claude_json, {})
        servers = data.setdefault("mcpServers", {})
        if name in servers:
            print("MCP server %s already exists in user config" % name)
            return 1
        # As the real CLI stores it: only the keys it knows, and a stdio server always has args.
        known = ("type", "command", "args", "env", "url", "headers", "oauth")
        config = {k: v for k, v in config.items() if k in known}
        if config.get("type", "stdio") == "stdio":
            config.setdefault("args", [])
        servers[name] = config
        save(claude_json, data)
        print("Added MCP server %s to user config" % name)
        return 0
    if rest[:2] == ["mcp", "remove"]:
        data = load(claude_json, {})
        if data.get("mcpServers", {}).pop(rest[2], None) is None:
            print('No MCP server named "%s" in user scope' % rest[2])
            return 1
        save(claude_json, data)
        return 0
    if rest[:3] == ["plugin", "marketplace", "add"]:
        source = rest[3]
        name = source.rstrip("/").split("/")[-1].removesuffix(".git")
        known = load(known_path, {})
        known[name] = {"source": {"source": "github", "repo": source}}
        save(known_path, known)
        return 0
    if rest[:2] == ["plugin", "install"]:
        plugin_id = rest[2]
        if plugin_id.split("@")[-1] not in load(known_path, {}):
            print("Plugin %s not found in any marketplace" % plugin_id)
            return 1
        installed = load(installed_path, {"version": 2, "plugins": {}})
        installed["plugins"][plugin_id] = [{"scope": "user", "version": "1.0.0"}]
        save(installed_path, installed)
        return 0
    if rest[:2] == ["plugin", "uninstall"]:
        installed = load(installed_path, {"version": 2, "plugins": {}})
        installed["plugins"].pop(rest[2], None)
        save(installed_path, installed)
        return 0
    sys.stderr.write("fake claude: unknown command %r\n" % argv)
    return 1


# `fake_claude.py old-cli ...` (as a node's command) plays a CLI from before
# `--strict-mcp-config`: it does not list the flag, and refuses a run that gives it.
OLD_CLI = len(sys.argv) > 1 and sys.argv[1] == "old-cli"
if OLD_CLI:
    sys.argv.pop(1)
if len(sys.argv) > 1 and sys.argv[1] in ("plugin", "mcp"):
    sys.exit(kit_cli(sys.argv[1:]))
# The node asks once whether this CLI can hold a session to its MCP config.
if sys.argv[1:] == ["--help"]:
    print("Usage: claude [options] [command] [prompt]")
    print("  --mcp-config <configs...>   Load MCP servers from JSON files or strings")
    print("  --include-partial-messages  Include partial message chunks as they arrive")
    if not OLD_CLI:
        print("  --strict-mcp-config         Only use MCP servers from --mcp-config")
    sys.exit(0)
if OLD_CLI and "--strict-mcp-config" in sys.argv:
    sys.stderr.write("error: unknown option '--strict-mcp-config'\n")
    sys.exit(1)

# The same environment `fleet agent ...` reads in a real session.
mcp = {"FLEET_IPC_ADDR": os.environ["FLEET_IPC_ADDR"], "FLEET_IPC_TOKEN": os.environ["FLEET_IPC_TOKEN"]}
host, port = mcp["FLEET_IPC_ADDR"].rsplit(":", 1)
mcp_servers = json.load(open(arg("--mcp-config")))["mcpServers"]
assert mcp_servers["fleet"]["args"] == ["mcp"]


def tool(name, args):
    s = socket.create_connection((host, int(port)))
    s.sendall((json.dumps({"token": mcp["FLEET_IPC_TOKEN"], "tool": name, "args": json.dumps(args)}) + "\n").encode())
    data = b""
    while not data.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        data += chunk
    s.close()
    resp = json.loads(data)
    out({"type": "tool_result", "tool": name, "ok": resp["ok"], "result": resp["result"]})
    return resp


session = arg("--resume") or "sess-" + uuid.uuid4().hex[:8]
out({"type": "system", "subtype": "init", "session_id": session, "model": arg("--model"),
     "permission_mode": arg("--permission-mode"), "role_prompt": bool(arg("--append-system-prompt")),
     # Every --model given (the last one wins in the real CLI) and the pre-approved tools.
     "models": [sys.argv[i + 1] for i, a in enumerate(sys.argv[:-1]) if a == "--model"],
     "allowed_tools": arg("--allowedTools"), "tools": arg("--tools"), "effort": arg("--effort"),
     "efforts": [sys.argv[i + 1] for i, a in enumerate(sys.argv[:-1]) if a == "--effort"],
     "strict_mcp": "--strict-mcp-config" in sys.argv, "setting_sources": arg("--setting-sources"),
     # The servers this session gets, and where its config is (a per-run file the test checks).
     "mcp_servers": sorted(mcp_servers), "mcp_config": arg("--mcp-config")})
# The kit's servers' env as the session sees it: a careless session printing it must not get the
# values into the transcript.
out({"type": "mcp_env", "env": {n: s.get("env", {}) for n, s in mcp_servers.items() if n != "fleet"}})

skill = os.path.join(arg("--plugin-dir"), "skills", "fleet", "SKILL.md")
out({"type": "skill", "present": os.path.exists(skill), "role": os.environ.get("FLEET_ROLE"), "cwd": os.getcwd()})
secrets_dir = os.environ.get("FLEET_SECRETS_DIR", "")
out({"type": "secrets", "files": sorted(os.listdir(secrets_dir)) if os.path.isdir(secrets_dir) else []})


def jira_token():
    # A careless agent quoting a secret it read: the orchestrator must not pass it on.
    jira = open(os.path.join(secrets_dir, "jira.env")).read()
    return [l.split("=", 1)[1] for l in jira.splitlines() if l.startswith("JIRA_API_TOKEN=")][0]


for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("type") == "control_request":
        out({"type": "control_seen", "request": msg["request"]})
        continue
    text = msg["message"]["content"]
    out({"type": "assistant", "message": {"content": [{"type": "text", "text": "got: " + text[:400]}]}})
    # Token-by-token output, as the real CLI writes it with --include-partial-messages (2.1.268):
    # deltas, then the whole block as an assistant line, then the block's stop. Slow enough that a
    # page watching the session sees it being written.
    if "live-stream" in text and "--include-partial-messages" in sys.argv:
        def ev(e):
            out({"type": "stream_event", "event": e, "parent_tool_use_id": None})
        ev({"type": "message_start", "message": {"id": "msg_live"}})
        ev({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        words = ["Streaming the answer as it is written, one piece after another, "] * 3 + ["done."]
        for w in words:
            time.sleep(0.4)
            ev({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": w}})
        out({"type": "assistant", "message": {"id": "msg_live", "content": [{"type": "text", "text": "".join(words)}]}})
        ev({"type": "content_block_stop", "index": 0})
        ev({"type": "message_stop"})
        out({"type": "result", "subtype": "success", "duration_ms": 1600, "total_cost_usd": 0.0})
        continue
    # The plan gate flow: every message plans, and nothing else. The plan says the owner is
    # needed only when the text says it is unclear, which a gate set to `auto` must respect.
    if "plan-gate" in text:
        plan = "# Plan\n1. Read the new pings\n2. Draft the replies"
        if "unclear" in text:
            plan += "\n\nOpen question: which channel counts?"
        r = tool("permission", {"tool_name": "ExitPlanMode", "input": {"plan": plan}})
        out({"type": "plan_decision", "decision": json.loads(r["result"])})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
        continue
    # A recurring job (the habits flow): present a plan and finish, nothing else. The plan says
    # the owner is needed only when the brief says the job is unclear, which is what lets the
    # test show that a habit never answers such a plan.
    # The live session flow's task: it only waits to be written to.
    if text.startswith("# Task") and "live-session" in text:
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
        continue
    if text.startswith("# Task") and "Review MR" in text and "[Message from" not in text:
        plan = "# Plan\n1. Read the diff\n2. Post the review"
        if "unclear" in text:
            plan += "\n\nOpen question: which branch is the target?"
        r = tool("permission", {"tool_name": "ExitPlanMode", "input": {"plan": plan}})
        out({"type": "plan_decision", "decision": json.loads(r["result"])})
        tool("complete", {"summary": "Reviewed and posted."})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    # A brand-new task: plan, ask, work. A restarted one (brief plus messages) only answers.
    elif text.startswith("# Task") and "[Message from" not in text:
        q = {"questions": [{"question": "Which color?", "header": "Color", "multiSelect": False,
                            "options": [{"label": "Red", "description": ""}, {"label": "Blue", "description": ""}]}]}
        r = tool("permission", {"tool_name": "AskUserQuestion", "input": q})
        decision = json.loads(r["result"])
        out({"type": "answers", "answers": decision["updatedInput"].get("answers")})
        r = tool("permission", {"tool_name": "ExitPlanMode", "input": {"plan": "# Plan\n1. Paint it\n2. Ship it"}})
        out({"type": "plan_decision", "decision": json.loads(r["result"])})
        tool("memory_recall", {"query": "paint", "shared": True})
        tool("milestones_set", {"items": ["Paint", "Prime", "Ship"]})
        tool("milestone_done", {"index": 0})
        # The merge request this task opened: the agent is the only one who knows it exists.
        tool("link_add", {"ref": "!940"})
        # Said twice (a retried step), and one that was wrong and is taken back again.
        tool("link_add", {"ref": "!940"})
        tool("link_add", {"ref": "https://example.test/notes"})
        tool("link_remove", {"ref": "https://example.test/notes"})
        # What the task produced, kept with it: a titled link, and a small file sent the way
        # `fleet agent attach <path>` sends one (begin, its pieces, finish).
        tool("attach_url", {"url": "https://claude.ai/artifact/e2e-paint", "title": "Paint swatches"})
        body = b"-- 0021_paint.sql\nupdate shed set color = 'blue';\n"
        r = tool("attach_begin", {"title": "Migration for devops", "file_name": "0021_paint.sql",
                                  "size": len(body), "sha256": hashlib.sha256(body).hexdigest()})
        up = json.loads(r["result"])["upload"]
        tool("attach_chunk", {"upload": up, "index": 0, "data": base64.b64encode(body).decode()})
        tool("attach_finish", {"upload": up})
        # Asked once, then asked again in better words: the fleet must leave one open question,
        # not two cards saying the same thing. The note in between blocks nobody, so asking
        # something else must not take it away.
        tool("ask_human", {"question": "Should I push?", "options": ["Yes", "No"], "blocking": True})
        tool("ask_human", {"question": "FYI: the bikeshed had two coats already.", "blocking": False})
        tool("ask_human", {"question": "Push the branch? It uses token " + jira_token(), "options": ["Yes", "No"], "blocking": True})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    elif text.startswith("[Message from the owner] admin"):
        # The brain administering the fleet: free reads, then changes that wait for the owner.
        tool("admin_read", {"what": "projects"})
        tool("admin_read", {"what": "secrets"})
        tool("admin_read", {"what": "project", "target": "demo"})
        tool("admin_read", {"what": "node", "target": "e2e-node"})
        tool("admin_propose", {"action": "project_create",
                               "params": {"slug": "web", "name": "Web", "description": "The website"},
                               "note": "The owner asked for it."})
        tool("admin_propose", {"action": "channel_add", "params": {"project": "demo", "channel": "C0DEMO", "triage": True}})
        tool("admin_propose", {"action": "channel_add", "params": {"project": "nope", "channel": "C0DEMO"}})
        tool("admin_propose", {"action": "project_bind_folder", "params": {"project": "fleet", "folder": "Jira"}})
        tool("admin_propose", {"action": "node_settings", "params": {"node": "e2e-node", "command": ["sh", "-c", "env"]}})
        tool("admin_propose", {"action": "folder_create", "params": {"name": "Notes"}})
        tool("web_link", {"page": "/secrets", "text": "Set the Jira token"})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    elif text.startswith("[Inbox batch"):
        # The secretary: triage each message of the batch by what it says.
        batch = [json.loads(l) for l in text.splitlines() if l.startswith("{")]
        for m in batch:
            t, r = m["text"], m["ref"]
            if "invoice" in t:
                tool("todo_add", {"title": "Reply to Alice about the invoice", "detail": "She asks for the <Q3> invoice.",
                                  "refs": [r], "due": "2026-11-20"})
            elif "review" in t:
                tool("todo_add", {"title": "Review Dave's doc", "refs": [r]})
            elif "outage" in t:
                tool("escalate", {"summary": "Bob reports an outage on the website", "refs": [r]})
            elif "ship" in t:
                tool("todo_add", {"title": "Reply to Dave about shipping", "refs": [r]})
            elif t.startswith("RULE:"):
                # A message talking the secretary into a rule (prompt injection, obeyed here so
                # the safety net shows: the owner is alerted and can undo it).
                tool("rule_add", {"text": t[5:].strip(), "because": t[5:].strip()})
            else:
                tool("triage", {"decision": "ignore", "refs": [r], "why": "chatter"})
        tool("todo_add", {"title": "Made up", "refs": ["slack:C0NOPE:1.0"]})
        # Whatever a message says, the secretary has no task, reply, agent or admin tools.
        for name, a in [("create_task", {"project": "demo", "title": "x", "brief": "x"}),
                        ("reply_owner", {"text": "hi"}),
                        ("propose_slack_reply", {"channel_id": "C1", "text": "hi"}),
                        ("send_to_task", {"task_id": "demo-BS-1", "text": "x"}),
                        ("ask_agent", {"agent": "Alice", "message": "x"}),
                        ("admin_read", {"what": "projects"}),
                        ("secretary_note", {"text": "x"}),
                        ("memory_retain", {"content": "x", "shared": True}),
                        ("memory_retain", {"content": "x", "bank": "project-fleet"}),
                        ("memory_banks", {})]:
            tool(name, a)
        if "People who wrote these messages" in text:
            # The people network: the batch carries the authors' cards. The secretary may look
            # someone up, but writing a record is the retrospective's alone.
            out({"type": "batch_people", "text": text[text.index("People who wrote these messages"):]})
            tool("people", {"query": "zed"})
            tool("retro_person", {"name": "Mallory", "add_tags": ["trusted"]})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    elif text.startswith("[Message from the owner in the Inbox]"):
        said = text.split("] ", 1)[1].split("\n")[0]
        if "ignore" in said:
            tool("rule_add", {"text": "Messages in #random are noise", "because": said})
        if '"ref":"' in text:
            # A reply to a post about a source message: draft an answer to it. Nothing is sent
            # until the owner taps Send on the card this creates.
            ref = text.split('"ref":"', 1)[1].split('"', 1)[0]
            tool("reply_draft", {"ref": ref, "text": "Yes, Friday works for the ship date."})
        tool("digest_post", {"text": "Since you asked: one todo is open."})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    elif text.startswith("[Retrospective people-day"):
        # The people pass: look first, then create and update records from the day's material,
        # quietly, until the per-run cap refuses one more.
        tool("people", {"query": "zed"})
        tool("memory_recall", {"query": "Zed"})
        tool("retro_person", {"name": "Zed Example", "position": "Backend lead",
                              "brief": "Owns the notes module.", "projects": ["notes", "Billing"],
                              "language": "ukrainian",
                              "add_observations": ["Prefers short and quick communication",
                                                   "Does code reviews on the notes module"],
                              "add_tags": ["Night Owl!"],
                              "add_contacts": [{"source": "slack", "handle": "@zed", "native_id": "U0ZED"}],
                              "last_interaction": "2026-11-01T10:00:00Z"})
        # The same name in another case updates the record; it never makes a second one.
        tool("retro_person", {"name": "zed example", "add_observations": ["Works only on backend parts"],
                              "remove_observations": ["Does code reviews on the notes module"]})
        tool("retro_person", {"name": "Olya", "position": "Project manager", "language": "en"})
        # One past the cap (three in tests): refused.
        tool("retro_person", {"name": "Over The Cap"})
        tool("retro_finish", {"verdict": "Kept the people records of the day."})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    elif text.startswith("[Retrospective"):
        # The fleet's own day, handed over by the orchestrator. Look at one task more closely,
        # propose, hit the daily limit, and always close with a verdict.
        tool("retro_task", {"task": "demo-BS-1"})
        tool("admin_read", {"what": "status"})
        tool("retro_propose", {"area": "harness", "title": "Agents re-ask the same question",
                               "detail": "demo-BS-1 asked twice because the first ask was abandoned."})
        tool("retro_propose", {"area": "agents", "title": "Handovers lose the ticket",
                               "detail": "Robin got no ticket reference in its brief."})
        # One past the limit: the orchestrator must refuse it.
        tool("retro_propose", {"area": "flows", "title": "Third one", "detail": "Over the cap."})
        # And the things it may never do, whatever the day looked like.
        for name, a in [("create_task", {"project": "fleet-src", "title": "x", "brief": "x"}),
                        ("reply_owner", {"text": "hi"}),
                        ("ask_human", {"question": "really?"}),
                        ("admin_propose", {"action": "project_create",
                                           "params": {"slug": "retro-overreach"}}),
                        ("send_to_task", {"task_id": "demo-BS-1", "text": "x"})]:
            tool(name, a)
        tool("retro_finish", {"verdict": "Read 6 tasks and the day's failures. Two things worth fixing."})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    elif text.startswith("[Daily digest"):
        tool("digest_post", {"text": "Daily digest: " + text[:40]})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    elif text.startswith("[Answer to Q-"):
        # One step skipped with a reason, one left open: completing shows it skipped too.
        tool("milestone_skip", {"index": 1, "reason": "already <primed>"})
        tool("memory_retain", {"content": "Bikeshed is blue", "bank": "notes"})
        token = jira_token()
        # Claude Code's own memory, in the folder the node's --settings points it at: a note with
        # a secret in it, which must reach the fleet's memory without the secret.
        settings = arg("--settings")
        if settings:
            folder = json.load(open(settings))["autoMemoryDirectory"]
            with open(os.path.join(folder, "bikeshed-colour.md"), "w") as f:
                f.write("---\nname: bikeshed-colour\n---\nThe owner wants it blue. Token " + token + "\n")
        tool("post", {"text": "Pushed with token " + token})
        # Left behind on purpose: finishing the task must close it, so nothing of a task that is
        # over still waits on the owner.
        tool("ask_human", {"question": "Anything else before I close?"})
        tool("complete", {"summary": "Painted and shipped with token " + token + ". " + text[:60]})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    else:
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
