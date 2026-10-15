#!/usr/bin/env python3
"""Check the deployed Pages artifact anonymously from an unrelated origin."""
import hashlib
import json
import sys
import time
from urllib.request import Request, urlopen


def download(url, limit):
    request = Request(url, headers={
        'Origin': 'https://example.org', 'Cache-Control': 'no-cache',
        'User-Agent': 'ubus-zero-public-download-check',
    })
    with urlopen(request, timeout=20) as response:
        if response.headers.get('Access-Control-Allow-Origin') != '*':
            raise ValueError(f'{url} does not allow anonymous downloads from every origin')
        body = response.read(limit + 1)
        if len(body) > limit:
            raise ValueError(f'{url} exceeds the expected size')
        return body


def verify(base_url, revision):
    metadata = json.loads(download(base_url + 'latest.json', 8192))
    if metadata['revision'] != revision or metadata['dirty']:
        raise ValueError('the deployed firmware is not the expected clean main build')
    binary = download(base_url + 'router-display.bin', 4 * 1024 * 1024)
    if len(binary) != metadata['size'] or hashlib.sha256(binary).hexdigest() != metadata['sha256']:
        raise ValueError('the deployed firmware does not match its size and SHA-256')
    print(f'Anonymous GET and CORS * verified: {base_url}router-display.bin')
    print(f'{revision}: {len(binary)} bytes; SHA-256 {metadata["sha256"]}')


if __name__ == '__main__':
    for attempt in range(6):
        try:
            verify(sys.argv[1].rstrip('/') + '/', sys.argv[2])
            break
        except Exception:
            if attempt == 5:
                raise
            time.sleep(5)
