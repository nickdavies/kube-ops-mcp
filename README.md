# kube-ops-mcp

An MCP server for the Kubernetes operations an agent needs but RBAC can't
grant safely.

| Tool | Why it needs a server |
|---|---|
| `dry_run_apply` | RBAC has no dry-run verb, so a dry-run needs real create/patch on every kind. The response is the merged object, which would echo back Secret data the caller can't read. The server holds the verbs and refuses Secrets. |
| `secret_keys` | Lists a Secret's key names. RBAC can only grant `get`, which returns the values. |

Operations that RBAC plus a ValidatingAdmissionPolicy *can* express
(`flux reconcile`, ExternalSecret force-sync) stay out of this server; grant
them to the agent directly so it uses the normal CLIs.

## Auth

Callers send a projected ServiceAccount token as `Authorization: Bearer`. The
server checks it with a TokenReview against `TOKEN_AUDIENCE`, then requires
the username to be in `ALLOWED_USERS`. Using a dedicated audience means the
caller's token is useless against the API server, and the API server's tokens
are useless here.

## Configuration

| Variable | Default | |
|---|---|---|
| `ALLOWED_USERS` | required | Comma-separated, e.g. `system:serviceaccount:workspaces:ws-homelab` |
| `ALLOWED_HOSTS` | required | Comma-separated `Host` values the MCP endpoint accepts (DNS-rebinding guard) |
| `TOKEN_AUDIENCE` | `kube-ops-mcp` | |
| `LISTEN_ADDR` | `0.0.0.0:8080` | |
| `RUST_LOG` | `info` | |

MCP is served at `/mcp` (streamable HTTP, stateless); `/healthz` is
unauthenticated.

## RBAC the server needs

- `create` on `authentication.k8s.io/tokenreviews`
- `get` on everything, `create`/`patch` on everything (dry-run only), and
  `bind`/`escalate` on RBAC kinds so dry-runs of Roles and bindings behave as
  they would for Flux
- `get` on `secrets` for `secret_keys`

Pair the write verbs with a ValidatingAdmissionPolicy that rejects any
non-dry-run write from the server's ServiceAccount, so a bug here can't
mutate the cluster.

## Development

```
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```
