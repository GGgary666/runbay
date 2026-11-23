"""Ask one question about the sample Markdown through a temporary Linux Runbay daemon."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.request import ProxyHandler, Request, build_opener


WORKSPACE = Path(__file__).resolve().parent / "cowiki"
QUESTION = "这份论文笔记里的主要评测指标是什么？"


def ask(ready, question):
    control = build_opener(ProxyHandler({}))
    base = "http://" + ready["addr"]
    token = Path(ready["token_file"]).read_text().strip()
    headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}

    def post(path, data):
        request = Request(base + path, json.dumps(data).encode(), headers)
        with control.open(request, timeout=10) as response:
            return json.load(response)

    post("/agents", {"agent_id": "wiki", "runtime": "cowiki", "workspace_dir": str(WORKSPACE)})
    run = post("/agents/wiki/run", {"prompt": question, "allowed_model": "deepseek-flash", "timeout_secs": 90})
    request = Request(f"{base}/runs/{run['run_id']}/events", headers=headers)
    answer = None
    transport = None
    with control.open(request, timeout=100) as events:
        for line in events:
            if not line.startswith(b"data: "):
                continue
            event = json.loads(line[6:])
            kind, data = event["event_type"], event["data"]
            if kind == "run.started":
                transport = data["gateway_transport"]
            if kind == "agent.event" and data.get("type") == "cowiki.answer":
                answer = data
            if kind == "run.succeeded" and answer:
                return {
                    "run_id": run["run_id"], "workspace_rev": run["workspace_rev"],
                    "gateway_transport": transport, **answer,
                }
            if kind in ("run.failed", "run.cancelled"):
                raise RuntimeError(f"{kind}: {data.get('error_code')}")
    raise RuntimeError("Run ended without a completed answer")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("question", nargs="?", default=QUESTION)
    parser.add_argument("--runbayd", type=Path, default=WORKSPACE.parents[1] / "target/debug/runbayd")
    args = parser.parse_args()
    if sys.platform != "linux" or not os.environ.get("RUNBAY_UPSTREAM_API_KEY"):
        parser.error("Run on Linux with RUNBAY_UPSTREAM_API_KEY set for the daemon")
    with tempfile.TemporaryDirectory(prefix="runbay-wiki-") as state:
        command = [
            str(args.runbayd.resolve()), "serve", "--port", "0", "--state-dir", state,
            "--enable-cowiki-adapter", "--allow-workspace-root", str(WORKSPACE),
            "--upstream-url", os.environ.get("RUNBAY_UPSTREAM_URL", "https://api.deepseek.com"),
        ]
        with subprocess.Popen(command, stdout=subprocess.PIPE, text=True) as daemon:
            try:
                line = daemon.stdout.readline()
                if not line.startswith("RUNBAYD_READY "):
                    raise RuntimeError("Daemon failed to start")
                result = ask(json.loads(line.split(" ", 1)[1]), args.question)
                print(json.dumps(result, ensure_ascii=False, indent=2))
            finally:
                daemon.terminate()
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()


if __name__ == "__main__":
    main()
