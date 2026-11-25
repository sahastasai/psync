#!/usr/bin/env python3
"""A GitHub release mirror for the benchmark: serves DIR/<basename of the path> for any path."""
import http.server, os, sys

root, port = sys.argv[1], int(sys.argv[2])


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        path = os.path.join(root, os.path.basename(self.path.split("?")[0]))
        if not os.path.isfile(path):
            self.send_error(404)
            return
        data = open(path, "rb").read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
