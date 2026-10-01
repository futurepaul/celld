# Container conformance

One Durable Object calls every method of `ctx.container` and answers a JSON
verdict per method: `ok`, a failure with its error, or `unsupported` when the
engine rejects the call as one it does not have. The same Worker runs on
Cloudflare, on celld with Docker, and on celld with the krun engine.

```sh
celld dev examples/container-conformance --port 9877
curl -s http://127.0.0.1:9877/run
```

On a krun node, set `CELLD_CONTAINER_ENGINE=krun:<engine socket>`. A node
without the `docker` CLI names an image another machine built and loaded into
the engine: `"images": {"base": {"image": "celld-image:<sha256>"}}`.

The container (`server.py`) serves `/`, `/env`, `/fetch?url=` (an outbound
request from inside, for `enableInternet` and the intercepts), and `/write` and
`/read` (a marker in the writable root, for the snapshot). It exits with 42 on
SIGTERM, so `signal()` and `monitor()` can be told apart.
