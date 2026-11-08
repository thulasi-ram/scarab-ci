#!/usr/bin/env python3
"""Render the Scarab mark's PNG icons in a browser and save them under ui/brand.

    python3 ui/brand/mark/serve_png.py [--no-open] [--port 8766]

Starts a local server on 127.0.0.1, opens mark/raster.html (which draws each
SVG onto a canvas at every size and posts the PNG back), saves the files, and
exits when the page reports it is done. Then run `generate.py --ico` to build
favicon.ico from the favicon PNGs.

The server only accepts .png files under the folders below.
"""
import http.server
import os
import sys
import threading
import urllib.parse
import webbrowser

BRAND = os.path.realpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
ALLOWED = ('icons/ink/', 'icons/cream/', 'icons/tile/', 'icons/tile-emerald/', 'favicon/')

class Handler(http.server.SimpleHTTPRequestHandler):
    saved = 0

    def __init__(self, *a, **k):
        super().__init__(*a, directory=BRAND, **k)

    def _reply(self, code, body=b''):
        self.send_response(code); self.end_headers(); self.wfile.write(body)

    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        if url.path == '/done':
            self._reply(200, b'ok')
            print(f'saved {Handler.saved} PNGs')
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        rel = urllib.parse.parse_qs(url.query).get('path', [''])[0]
        dest = os.path.realpath(os.path.join(BRAND, rel))
        if url.path != '/save' or not rel.startswith(ALLOWED) or not rel.endswith('.png') or not dest.startswith(BRAND + os.sep):
            return self._reply(400, b'refused')
        data = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with open(dest, 'wb') as f: f.write(data)
        Handler.saved += 1
        self._reply(200, str(len(data)).encode())

    def log_message(self, *a): pass

def main(argv):
    port = int(argv[argv.index('--port') + 1]) if '--port' in argv else 8766
    server = http.server.ThreadingHTTPServer(('127.0.0.1', port), Handler)
    url = f'http://127.0.0.1:{port}/mark/raster.html'
    print('serving', url)
    if '--no-open' not in argv: webbrowser.open(url)
    server.serve_forever()

if __name__ == '__main__':
    main(sys.argv[1:])
