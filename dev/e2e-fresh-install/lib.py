import json, urllib.request, urllib.error
def hdr(client="e2e", device_id="e2e", token=None):
    h = f'MediaBrowser Client="{client}", Device="E2E", DeviceId="{device_id}", Version="1"'
    return h + (f', Token="{token}"' if token else "")
def call(base, method, path, body=None, client="e2e", device_id="e2e", token=None):
    req = urllib.request.Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": hdr(client, device_id, token)})
    try:
        with urllib.request.urlopen(req, timeout=90) as r:
            raw = r.read(); return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:200]
