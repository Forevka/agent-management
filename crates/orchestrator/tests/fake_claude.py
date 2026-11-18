#!/usr/bin/env python3
"""A stand-in for `claude -p --input-format stream-json --output-format stream-json`.

It follows the fleet's headless contract closely enough for end-to-end tests: it prints an
init event with a session id, reads user messages and control requests from stdin, and calls
fleet tools through the node's loopback IPC exactly as the `fleet mcp` shim would (the shim
itself is tested separately).
"""
import json
import os
import socket
import sys
import uuid


def out(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def arg(name):
    argv = sys.argv
    return argv[argv.index(name) + 1] if name in argv else None


# The same environment `fleet agent ...` reads in a real session.
mcp = {"FLEET_IPC_ADDR": os.environ["FLEET_IPC_ADDR"], "FLEET_IPC_TOKEN": os.environ["FLEET_IPC_TOKEN"]}
host, port = mcp["FLEET_IPC_ADDR"].rsplit(":", 1)
assert json.load(open(arg("--mcp-config")))["mcpServers"]["fleet"]["args"] == ["mcp"]


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
     "permission_mode": arg("--permission-mode"), "role_prompt": bool(arg("--append-system-prompt"))})

skill = os.path.join(arg("--plugin-dir"), "skills", "fleet", "SKILL.md")
out({"type": "skill", "present": os.path.exists(skill), "role": os.environ.get("FLEET_ROLE")})
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
    # A brand-new task: plan, ask, work. A restarted one (brief plus messages) only answers.
    if text.startswith("# Task") and "[Message from" not in text:
        q = {"questions": [{"question": "Which color?", "header": "Color", "multiSelect": False,
                            "options": [{"label": "Red", "description": ""}, {"label": "Blue", "description": ""}]}]}
        r = tool("permission", {"tool_name": "AskUserQuestion", "input": q})
        decision = json.loads(r["result"])
        out({"type": "answers", "answers": decision["updatedInput"].get("answers")})
        r = tool("permission", {"tool_name": "ExitPlanMode", "input": {"plan": "# Plan\n1. Paint it\n2. Ship it"}})
        out({"type": "plan_decision", "decision": json.loads(r["result"])})
        tool("memory_recall", {"query": "paint", "shared": True})
        tool("milestones_set", {"items": ["Paint", "Ship"]})
        tool("milestone_done", {"index": 0})
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
    elif text.startswith("[Answer to Q-"):
        tool("milestone_done", {"index": 1})
        tool("memory_retain", {"content": "Bikeshed is blue", "bank": "notes"})
        token = jira_token()
        tool("post", {"text": "Pushed with token " + token})
        tool("complete", {"summary": "Painted and shipped with token " + token + ". " + text[:60]})
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
    else:
        out({"type": "result", "subtype": "success", "total_cost_usd": 0.0})
