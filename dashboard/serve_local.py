#!/usr/bin/env python3
"""
Local Dashboard Server
Binds strictly to 127.0.0.1 (Localhost only) to respect hard constraints:
'No ports shall be exposed to the public internet.'
"""

import http.server
import socketserver
import sys
from pathlib import Path

# Add dashboard folder to path
sys.path.insert(0, str(Path(__file__).parent))
from generate_dashboard import main as generate_main

PORT = 8080
HOST = "127.0.0.1"  # STRICT LOCALHOST BINDING ONLY


class CustomHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        dashboard_dir = str(Path(__file__).parent)
        super().__init__(*args, directory=dashboard_dir, **kwargs)

    def do_GET(self):
        # Refresh dashboard data on each page load
        try:
            generate_main()
        except Exception as e:
            print(f"Error refreshing dashboard: {e}")
        return super().do_GET()


def run_server():
    generate_main()
    with socketserver.TCPServer((HOST, PORT), CustomHandler) as httpd:
        print(f"Dashboard server live at http://{HOST}:{PORT}/index.html")
        print("Binding restricted to 127.0.0.1. Press Ctrl+C to terminate.")
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            print("\nShutting down dashboard server.")


if __name__ == "__main__":
    run_server()
