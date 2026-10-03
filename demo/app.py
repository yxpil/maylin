# Maylin 演示实例：Python HTTP 服务
# 用法: python app.py --port 3002
import argparse
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def get_port():
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=0)
    args, _ = p.parse_known_args()
    return args.port or int(os.environ.get("PORT", 3000))

PORT = get_port()

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/health"):
            body = json.dumps({"ok": True, "pid": os.getpid(), "port": PORT}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
        else:
            body = f"hello from python-demo (pid={os.getpid()}, port={PORT})\n".encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        sys.stdout.write("[python-demo] %s\n" % (fmt % args))
        sys.stdout.flush()

if __name__ == "__main__":
    print(f"[python-demo] listening on {PORT}, pid={os.getpid()}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
