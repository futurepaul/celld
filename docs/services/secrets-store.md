# Secrets Store

A Worker reads a secret of the
[Secrets Store](https://developers.cloudflare.com/secrets-store/) through a
binding: `await env.HOST_SECRET.get()` answers its value. On celld the binding is
the same, a `Fetcher` whose `get()` answers the value, and the value comes from
the node's secrets backend, which the node's operator names. The Worker never
sees the backend, its address or its credentials: they are the node's
configuration, not the Worker's variables, and the host reads the secret outside
every isolate.

## Example

[`crates/celld/tests/cloudflare_surface.rs`](../../crates/celld/tests/cloudflare_surface.rs)
binds two secrets on a `celld dev` node with a `vars` file and reads them: one
held, one not.

## Configuration

A `secrets_store_secrets` entry gives a `binding`, a `store_id` and a
`secret_name`, as Wrangler takes it. celld reads the secret by its `secret_name`
(at most 255 letters, digits, `-` and `_`); a node has one store, so the
`store_id` is kept and not read. Two bindings may name one secret. A config binds
at most 128.

`CELLD_SECRETS_FILE` names the node's secrets file, an absolute path to a JSON
file that only its owner may read or write (mode `600`). It names one backend,
and nothing falls back to one it does not name:

```json
{
  "backend": "openbao",
  "addr": "http://127.0.0.1:8200",
  "mount": "fragment",
  "token_file": "/etc/celld/openbao.token"
}
```

`openbao` reads Vault's KV v2 API, OpenBao's or a company's HashiCorp Vault:
`GET <addr>/v1/<mount>/data/[<prefix>/]<secret_name>`, the value in its field
`value` (`bao kv put -mount=fragment fragment-host-secret value=…`). `prefix`
and `namespace` (`X-Vault-Namespace`) are optional. The token is `token_file`'s
(owner-only, read again at every read, so a token its owner renews or replaces
there is the one used) or `token`. Renewing the token is its owner's: an agent,
or the stack that minted it. A read takes at most 5 seconds and 256 KiB, uses the
roots a Worker trusts (with `CELLD_EXTRA_CA_FILE`'s), and never follows a
redirect, so the token goes nowhere else.

```json
{ "backend": "vars", "values": { "fragment-host-secret": "…" } }
```

`vars` holds the values in the file itself, for `celld dev` alone (development
and tests); a node of a fleet refuses it at start.

Unset, the node has no backend, and every `get()` rejects. The file is read once,
before the node starts, and a file that is wrong stops the start, saying why.

## Errors

A `get()` that cannot answer rejects with an `Error` whose `name` is
`SecretsError` and whose `code` says why:

| code | why |
| --- | --- |
| `unbound` | the node names no backend |
| `missing` | the backend holds no such secret (workerd rejects one too) |
| `refused` | OpenBao refused the token (403), or the token file is unreadable or readable by others |
| `sealed` | OpenBao is sealed, or not ready (503) |
| `down` | OpenBao is not answering, or did not answer in time |
| `failed` | OpenBao answered something else, or too much |
| `malformed` | the secret has no `value` field |

The message names the secret and the backend, never a value or a token. Nothing
is cached in celld; a Worker keeps what it read as long as it likes.

The deploy requires the `secrets-store-v1` feature, so a node that predates it
refuses it.
