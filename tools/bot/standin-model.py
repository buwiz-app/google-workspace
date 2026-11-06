#!/usr/bin/env python3
"""A stand-in for the model, on loopback, for test Bots: no real model, no key.

It speaks the Messages API's stream just enough to call one tool and record
what comes back. A chat message naming a tool in capitals drives it:

    CALL ui_look {}
    CALL gws_search_mail {"query": "from:ana"}

The tool's result is saved, byte for byte, to <dir>/result-<n>.txt: the exact
bytes a real model would have been sent. The tools it was offered go to
<dir>/tools.json. Usage: standin-model.py <port> <dir>
"""
import json, re, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, DIR = int(sys.argv[1]), sys.argv[2]
seen = {"n": 0}


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


def message(blocks, stop):
    events = [{"type": "message_start", "message": {"id": "msg_standin", "type": "message", "role": "assistant", "model": "standin", "content": [], "stop_reason": None, "usage": {"input_tokens": 1, "output_tokens": 1}}}]
    for i, b in enumerate(blocks):
        if b["type"] == "text":
            events += [{"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}, {"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": b["text"]}}]
        else:
            events += [{"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": b["id"], "name": b["name"], "input": {}}}, {"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": json.dumps(b["input"])}}]
        events.append({"type": "content_block_stop", "index": i})
    events += [{"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 1}}, {"type": "message_stop"}]
    return sse(events)


def texts(content):
    if isinstance(content, str):
        return content
    return "\n".join(c.get("text", "") for c in content if isinstance(c, dict) and c.get("type") == "text")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        with open(f"{DIR}/tools.json", "w") as f:
            json.dump(sorted(t.get("name", "") for t in body.get("tools", [])), f)
        last = (body.get("messages") or [{}])[-1]
        results = [c for c in last.get("content", []) if isinstance(c, dict) and c.get("type") == "tool_result"] if isinstance(last.get("content"), list) else []
        if results:
            seen["n"] += 1
            with open(f"{DIR}/result-{seen['n']}.txt", "w") as f:
                f.write(json.dumps(results))
            reply = message([{"type": "text", "text": "Here's what I found."}], "end_turn")
        else:
            m = re.search(r"CALL (\w+) (\{.*\})", texts(last.get("content", "")))
            if m:
                reply = message([{"type": "tool_use", "id": f"toolu_{seen['n']}", "name": m.group(1), "input": json.loads(m.group(2))}], "tool_use")
            else:
                reply = message([{"type": "text", "text": "Stand-in model."}], "end_turn")
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
