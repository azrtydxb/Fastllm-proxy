# Interactive API reference

Every endpoint, with its request and response shapes, from the same
`openapi.json` the running control plane serves at `GET /openapi.json`.

The spec is checked against the router by `tests/openapi.rs` in **both**
directions — a route with no spec entry fails the build, and so does a spec
entry whose route no longer exists — so this cannot drift from the code the
way a hand-written endpoint list does.

<div id="swagger-ui" style="margin: 0 -8px;"></div>
<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
<script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js" crossorigin></script>
<script>
  // `Try it out` is off: there is no server to send a request to from a
  // static docs site, and a button that always fails is worse than no button.
  // Point a browser at your own control plane's /docs for a live one.
  window.addEventListener("load", function () {
    if (!window.SwaggerUIBundle) return;
    var root = document.querySelector('meta[name="swagger-spec"]');
    SwaggerUIBundle({
      url: (root ? root.content : "openapi.json"),
      dom_id: "#swagger-ui",
      deepLinking: true,
      supportedSubmitMethods: [],
      defaultModelsExpandDepth: 0,
      docExpansion: "list",
      tryItOutEnabled: false,
    });
  });
</script>
<meta name="swagger-spec" content="../openapi.json">

## Against your own deployment

The control plane serves the same two routes, and there `Try it out` works:

| | |
|---|---|
| `GET /openapi.json` | The spec |
| `GET /docs` | Swagger UI |

Both sit on the **admin** listener (`:4001`) alongside `/healthz`, and both
are outside the session gate — a spec is not a secret and a probe target
cannot hold a cookie. Everything they *describe* under `/admin/*` still
requires a session and a per-route permission.

```bash
kubectl -n fastllm port-forward svc/fastllm-control 4001:4001
open https://localhost:4001/docs
```

## Generating a client

```bash
npx @openapitools/openapi-generator-cli generate \
  -i https://your-control-plane:4001/openapi.json \
  -g typescript-fetch -o ./fastllm-client
```

The gateway's own endpoints are OpenAI-shaped, so an OpenAI SDK is the better
client for those — see [Connecting a client](../integrations.md). This is for
the admin API, which has no SDK and is where a generated client earns its
keep.
