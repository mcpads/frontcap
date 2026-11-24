#!/usr/bin/env python3
"""Relays tool calls to one long-lived frontcap-mcp process, so an agent without MCP configuration can
drive it from a shell: the server process (and the leases it holds) lives as long as `serve`.

  mcp-relay.py serve <socket> <log>      start frontcap-mcp ($FRONTCAP_MCP, default the debug build) and
                                         listen (foreground); the environment, FRONTCAP_WORKSPACE
                                         included, passes to it
  mcp-relay.py discover <socket>         return the Server Instructions and the tool list
  mcp-relay.py call <socket> <tool> [json]  call a tool; returns {"is_error", "result", "images"?}

Client output above 16 KiB is saved as complete JSON in .frontcap/relay-responses/; stdout gives
only response_file, bytes, sha256 and is_error (null for discovery). --output <new-file> always
saves JSON; --inline explicitly opts into unbounded legacy stdout for machine consumers.
Files are on the CLI client's host. Read/filter that exact response file instead of calling again.

Every completed call, discovery response and rejected request is appended to <log> as one JSON line.
This records relay access, not filesystem access or proof that the client received an answer.
Images a tool returns inline are saved
next to the log (`images/<n>.<ext>`, never replacing an earlier one), and the answer lists their paths,
so the caller can view them. Calls may overlap: one sent while another runs (`status`, `session cancel`)
is answered as the server answers it.
"""
import argparse, base64, hashlib, itertools, json, os, socket, subprocess, sys, tempfile, threading
from pathlib import Path

IMAGE_NUMBERS = itertools.count(1)

def serve(path, log_path):
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    binary = os.environ.get("FRONTCAP_MCP") or os.path.join(root, "target/debug/frontcap-mcp")
    proc = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, cwd=root)
    lock = threading.Lock()
    counter = [0]
    # Calls run concurrently, as an MCP client may send them (a `session cancel` while a wait runs):
    # one reader hands each answer to the call with its id.
    pending = {}

    def read():
        for line in proc.stdout:
            try:
                answer = json.loads(line)
            except ValueError:
                continue
            waiter = pending.pop(answer.get("id"), None) if isinstance(answer, dict) else None
            if waiter is not None:  # anything else is a notification
                waiter[1].append(answer)
                waiter[0].set()
        sys.stderr.write("frontcap-mcp ended\n")
        os._exit(1)

    def request(method, params):
        done, slot = threading.Event(), []
        with lock:
            counter[0] += 1
            pending[counter[0]] = (done, slot)
            proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": counter[0], "method": method, "params": params}) + "\n")
            proc.stdin.flush()
        done.wait()
        return slot[0]

    threading.Thread(target=read, daemon=True).start()

    init = request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "relay", "version": "0"}})
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    proc.stdin.flush()
    discovery = {"instructions": init["result"].get("instructions", ""), "tools": request("tools/list", {})["result"]["tools"]}
    if os.path.exists(path):
        os.unlink(path)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    server.listen(16)
    log_lock = threading.Lock()

    def serve_one(conn):
        try:
            handle(conn, request, discovery, log_path, log_lock)
        except OSError:
            pass  # the client went away; the server and its leases stay

    while True:
        conn, _ = server.accept()
        threading.Thread(target=serve_one, args=(conn,), daemon=True).start()

def handle(conn, request, discovery, log_path, log_lock):
    def record(entry):
        with log_lock, open(log_path, "a") as log:
            log.write(json.dumps(entry) + "\n")

    with conn, conn.makefile("rw") as f:
        raw = f.readline()
        try:
            msg = json.loads(raw)
            if not isinstance(msg, dict) or not (msg.get("discover") or isinstance(msg.get("tool"), str)):
                raise ValueError("expected {\"tool\": ..., \"arguments\": ...}")
        except ValueError as e:
            answer = {"is_error": True, "result": {"code": "relay", "message": str(e)}}
            record({"event": "rejected", "request": raw, "answer": answer})
            f.write(json.dumps(answer) + "\n")
            f.flush()
            return
        if msg.get("discover"):
            answer = discovery
            record({"event": "discovery", "request": msg, "answer": answer})
        else:
            r = request("tools/call", {"name": msg["tool"], "arguments": msg.get("arguments", {})})
            result = r.get("result", {})
            answer = {"is_error": result.get("isError", True), "result": result.get("structuredContent", r.get("error"))}
            images = []
            for item in result.get("content", []):
                if item.get("type") == "image":
                    directory = os.path.join(os.path.dirname(os.path.abspath(log_path)), "images")
                    os.makedirs(directory, exist_ok=True)
                    extension = item.get("mimeType", "image/png").split("/")[-1]
                    # A relay started again with the same log keeps numbering past the images already there.
                    while True:
                        path = os.path.join(directory, f"{next(IMAGE_NUMBERS)}.{extension}")
                        try:
                            image = open(path, "xb")
                            break
                        except FileExistsError:
                            continue
                    with image:
                        image.write(base64.b64decode(item["data"]))
                    images.append(path)
            if images:
                answer["images"] = images
            record({"tool": msg["tool"], "arguments": msg.get("arguments", {}), "answer": answer})
        f.write(json.dumps(answer, ensure_ascii=False) + "\n")
        f.flush()

def send(path, msg):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    with s, s.makefile("rw") as f:
        f.write(json.dumps(msg) + "\n")
        f.flush()
        return f.readline()

def client_response(path, message, output=None, inline=False):
    # Reserve writable output before dispatch: an existing destination must never cause an
    # effect to run and then tempt the caller to repeat it just to retrieve its response.
    destination = None
    if not inline:
        if output is not None:
            destination = Path(output).absolute()
            stream = destination.open("xb")
        else:
            directory = Path.cwd() / ".frontcap/relay-responses"
            directory.mkdir(parents=True, exist_ok=True)
            fd, name = tempfile.mkstemp(prefix="response-", suffix=".json", dir=directory)
            destination = Path(name)
            stream = os.fdopen(fd, "wb")
    try:
        raw = send(path, message)
        data = raw.encode("utf-8")
        if destination is not None:
            # Preserve the wire response even if rendering or JSON decoding fails afterward.
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
            stream.close()
        if message.get("discover"):
            answer = json.loads(raw)
            rendered = ("SERVER INSTRUCTIONS:\n" + answer["instructions"] + "\n\nTOOLS:\n"
                        + json.dumps(answer["tools"], indent=1, ensure_ascii=False) + "\n")
        else:
            rendered = raw
        if destination is None:
            print(rendered, end="")
            return
        if output is None and len(rendered.encode("utf-8")) <= 16 * 1024:
            print(rendered, end="")
            destination.unlink()
        else:
            answer = json.loads(raw)
            print(json.dumps(dict(response_file=str(destination), bytes=len(data),
                                  sha256=hashlib.sha256(data).hexdigest(),
                                  is_error=answer.get("is_error"))))
    except BaseException:
        if destination is not None:
            stream.close()
            print(f"Response destination retained: {destination}. Execution may have occurred; "
                  "inspect original request status instead of repeating the action.", file=sys.stderr)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    server = commands.add_parser("serve")
    server.add_argument("socket")
    server.add_argument("log")
    for name in ("discover", "call"):
        client = commands.add_parser(name)
        client.add_argument("socket")
        if name == "call":
            client.add_argument("tool")
            client.add_argument("arguments", nargs="?", default="{}")
        output = client.add_mutually_exclusive_group()
        output.add_argument("--output", help="new JSON file; refuses existing destinations before dispatch")
        output.add_argument("--inline", action="store_true", help="unbounded legacy stdout for machine consumers")
    args = parser.parse_args()
    if args.command == "serve":
        serve(args.socket, args.log)
    else:
        message = ({"discover": True} if args.command == "discover" else
                   {"tool": args.tool, "arguments": json.loads(args.arguments)})
        client_response(args.socket, message, args.output, args.inline)


if __name__ == "__main__":
    main()
