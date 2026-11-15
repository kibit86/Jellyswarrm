# Fresh-install end-to-end test

Two brand-new Jellyfin 12.4 servers and an empty proxy, driven with Chrome (Playwright).
Server A is set up from scratch by the script; **server B is left untouched and is set up
in the browser, through the proxy** (its first-run wizard creates its first account).

```sh
# 1. image of the proxy and a Docker network
docker build -t jellyswarrm-test .            # from the repository root
docker network create jsw-e2e

# 2. working directories: a/ and b/ each get config, cache and media (a movie folder);
#    proxy/ holds jellyswarrm.toml with username "proxyadmin" / password "proxy-pw-e2e"
#    and the two servers http://jsw-jf-a:8096 and http://jsw-jf-b:8096 (see ../../docs)
docker run -d --name jsw-jf-a --network jsw-e2e -p 127.0.0.1:18096:8096 -v $PWD/a/config:/config -v $PWD/a/cache:/cache -v $PWD/a/media:/media:ro jellyfin/jellyfin:12.4.20261026-211555
docker run -d --name jsw-jf-b --network jsw-e2e -p 127.0.0.1:18097:8096 -v $PWD/b/config:/config -v $PWD/b/cache:/cache -v $PWD/b/media:/media:ro jellyfin/jellyfin:12.4.20261026-211555
python3 provision_a.py                        # sets up server A; server B stays brand new
docker run -d --name jsw-proxy --network jsw-e2e -p 127.0.0.1:18090:3000 -v $PWD/proxy:/app/data jellyswarrm-test

# 3. the browser test (Python venv with `playwright`; uses the system Chrome)
python3 browser_e2e.py ./shots
```

Prints one PASS/FAIL line per check and exits non-zero if any fails.
