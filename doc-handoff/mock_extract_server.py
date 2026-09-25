"""固定响应提取端点（doc4/07 端到端演示用）：对任意 Chat Completions 请求返回
空候选的固定 JSON。仅本机 127.0.0.1 演示，不调用任何真实模型。"""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

FIXED = {
    "choices": [{"message": {"content": json.dumps({"candidates": []})}}],
    "usage": {"prompt_tokens": 42, "completion_tokens": 3},
}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        body = json.dumps(FIXED).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", 3979), Handler).serve_forever()
