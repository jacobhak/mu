#!/usr/bin/env python3
"""Loopback fake of the Anthropic Messages API for the memory measurements.

usage: fake-anthropic.py PORT_FILE HOLD_SECONDS LOG_FILE

Writes its port to PORT_FILE. GET /v1/models lists one model. POST /v1/messages streams one text
delta, then holds the stream open for HOLD_SECONDS before finishing it. One log line per request.
"""
import http.server, json, sys, time

PORT_FILE, HOLD, LOG = sys.argv[1], float(sys.argv[2]), sys.argv[3]


def log(line):
    with open(LOG, "a") as f:
        f.write(line + "\n")


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        log(f"GET {self.path}")
        body = json.dumps({"data": [{"id": "claude-sonnet-5-5", "context_length": 200000, "max_output_tokens": 32000}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        log(f"POST {self.path} {len(body)} bytes key={'yes' if self.headers.get('x-api-key') else 'no'}")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def chunk(text):
            b = text.encode()
            self.wfile.write(f"{len(b):x}\r\n".encode() + b + b"\r\n")
            self.wfile.flush()

        chunk(sse("message_start", {"type": "message_start", "message": {
            "id": "m", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5", "content": [],
            "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 8, "output_tokens": 1}}}))
        chunk(sse("content_block_start", {"type": "content_block_start", "index": 0,
                                          "content_block": {"type": "text", "text": ""}}))
        chunk(sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                          "delta": {"type": "text_delta", "text": "pong"}}))
        time.sleep(HOLD)
        chunk(sse("content_block_stop", {"type": "content_block_stop", "index": 0}))
        chunk(sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                    "usage": {"output_tokens": 3}}))
        chunk(sse("message_stop", {"type": "message_stop"}))
        self.wfile.write(b"0\r\n\r\n")

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(PORT_FILE, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
