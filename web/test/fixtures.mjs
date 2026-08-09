// The admin API's responses, in the shape `control::api` actually serialises.
//
// Shared by both harnesses so there is one description of the wire format. It
// matters that this is the *wire* format and not the Rust field names: the
// first version of this file nested a rule's conditions under
// `match_condition` because that is what the struct field is called, while
// `RuleView` flattens them — so the screen looked right in the test and every
// real rule rendered as a catch-all.

const FIXTURES = {
  "/admin/config": {
    role: "all",
    version: "0.1.0",
    tls: false,
    uptime_seconds: 4210,
    config_poll_seconds: 5,
    health_report_interval_seconds: 10,
    cache_max_entries: 4096,
    cache_max_bytes: 67108864,
    otel_endpoint: null,
    otel_sample_one_in: 0,
    classifier_tier1: true,
    classifier_tier2: false,
    session_ttl_hours: 12,
    snapshot_rebuild_failures: 0,
    snapshot_version: 1786273263008852,
    models: 2,
    models_unpriced: 1,
    models_cached: 1,
  },
  // Two replicas that disagree about one backend and about the snapshot
  // version: the two states this UI exists to surface, so the banners and the
  // split-health rendering are both exercised rather than only the happy path.
  "/admin/fleet": [
    {
      replica: "proxy-1",
      snapshot_version: 1786273263008852,
      uptime_seconds: 90061,
      backends: [
        {
          api_base: "http://10.42.1.7:8000/v1",
          model: "qwen2.5-32b",
          healthy: true,
          inflight: 3,
          requests_total: 41000,
          errors_total: 2,
        },
        {
          api_base: "https://api.anthropic.com/v1",
          model: "claude-sonnet-4-5",
          healthy: true,
          inflight: 1,
          requests_total: 900,
          errors_total: 0,
        },
      ],
      process: {
        requests_ok: 41900,
        requests_failed: 2,
        cache_hits: 120,
        cache_misses: 380,
        cache_entries: 88,
        cache_bytes: 1048576,
        usage_dropped: 0,
      },
    },
    {
      replica: "proxy-2",
      snapshot_version: 1786273023008852,
      uptime_seconds: 300,
      backends: [
        {
          api_base: "http://10.42.1.7:8000/v1",
          model: "qwen2.5-32b",
          healthy: false,
          inflight: 0,
          requests_total: 10,
          errors_total: 4,
        },
      ],
      process: {
        requests_ok: 10,
        requests_failed: 4,
        cache_hits: 0,
        cache_misses: 6,
        cache_entries: 0,
        cache_bytes: 0,
        usage_dropped: 214,
      },
    },
  ],
  "/admin/models": [
    {
      id: 3,
      name: "local-qwen",
      description: "self-hosted",
      input_price_per_mtok: 0,
      output_price_per_mtok: 0,
      cache_ttl_seconds: 300,
      backends: [
        {
          id: 11,
          api_base: "http://10.42.1.7:8000/v1",
          upstream_model: "qwen2.5-32b",
          has_upstream_api_key: false,
          protocol: "openai",
          auth_header: "authorization",
          default_max_tokens: null,
        },
      ],
    },
    {
      id: 5,
      name: "claude-sonnet",
      description: "",
      input_price_per_mtok: null,
      output_price_per_mtok: null,
      cache_ttl_seconds: null,
      backends: [
        {
          id: 12,
          api_base: "https://api.anthropic.com/v1",
          upstream_model: "claude-sonnet-4-5",
          has_upstream_api_key: true,
          protocol: "anthropic",
          auth_header: "x-api-key",
          default_max_tokens: 4096,
        },
      ],
    },
  ],
  "/admin/virtual-models": [
    {
      id: 2,
      name: "gpt-router",
      description: "",
      rules: [
        {
          id: 7,
          position: 0,
          // Flattened, exactly as `RuleView` serialises it — there is no
          // `match_condition` key on the wire. The first version of this
          // fixture nested them, which made the screen look correct in the
          // test while every real rule rendered as a catch-all.
          principals: [],
          roles: ["engineering"],
          min_prompt_tokens: 256,
          stream: true,
          headers: { "x-fastllm-tier": "batch" },
          days: [],
          class: "coding",
          targets: [{ id: 21, model_id: 5, model: "claude-sonnet", weight: 80, position: 0 }],
        },
      ],
      default_targets: [{ id: 22, model_id: 3, model: "local-qwen", weight: 100, position: 0 }],
    },
  ],
  "/admin/principals": [
    {
      id: 1,
      kind: "user",
      name: "ops@kryton",
      email: "ops@kryton",
      disabled: false,
      created_at: "2026-01-01T00:00:00Z",
      roles: ["admin"],
    },
    {
      id: 2,
      kind: "service_account",
      name: "batch-etl",
      email: null,
      disabled: false,
      created_at: "2026-01-01T00:00:00Z",
      roles: ["inference"],
    },
  ],
  "/admin/roles": [
    {
      id: 1,
      name: "admin",
      description: "",
      permissions: [
        { verb: "usage:read", resource: "*" },
        { verb: "config:write", resource: "*" },
        // `model/*`, not `*`: the resource is namespaced and `validate_grant`
        // refuses a bare `*` for this verb. Verified against a live control
        // plane — writing this from assumption is what hid the last two bugs.
        { verb: "model:invoke", resource: "model/*" },
      ],
    },
    {
      id: 2,
      name: "inference",
      description: "",
      permissions: [{ verb: "model:invoke", resource: "model/local-qwen" }],
    },
  ],
  "/admin/keys": [
    {
      id: 31,
      prefix: "sk-abcd1234",
      name: "ci-runner",
      principal_id: 2,
      principal: "batch-etl",
      expires_at: "2026-12-01T00:00:00Z",
      disabled: false,
      created_at: "2026-01-01T00:00:00Z",
      last_used_at: "2026-08-09T10:00:00Z",
    },
    {
      id: 32,
      prefix: "sk-dead0000",
      name: "old",
      principal_id: 2,
      principal: "batch-etl",
      expires_at: null,
      disabled: true,
      created_at: "2026-01-01T00:00:00Z",
      last_used_at: null,
    },
  ],
  "/admin/limits": [
    { principal_id: 2, principal: "batch-etl", requests_per_min: 600, tokens_per_min: 400000 },
  ],
  "/admin/budgets": [
    {
      principal_id: 2,
      principal: "batch-etl",
      tokens_total: 12000000,
      tokens_used: 10100000,
      cost_total_micros: 500000000,
      cost_used_micros: 155000000,
      window: "daily",
      window_start: "2026-08-09T00:00:00Z",
    },
    {
      principal_id: 1,
      principal: "ops@kryton",
      tokens_total: null,
      tokens_used: 4,
      cost_total_micros: null,
      cost_used_micros: 0,
      window: "monthly",
      window_start: "2026-08-01T00:00:00Z",
    },
  ],
  "/admin/prompt-classes": [
    {
      id: 1,
      name: "coding",
      description: "",
      tier: "fast",
      min_margin: 0.08,
      refines: [],
      examples: 32,
      routable: true,
    },
    {
      id: 2,
      name: "summarise",
      description: "",
      tier: "refined",
      min_margin: null,
      refines: ["coding"],
      examples: 6,
      routable: false,
    },
  ],
  "/admin/fallback-model": { id: 3, name: "local-qwen" },
  "/admin/audit": [
    {
      id: 1361,
      actor_name: "ops@kryton",
      actor_id: 1,
      action: "POST",
      target: "/admin/keys",
      detail: { status: 200 },
      at: "2026-08-09T09:00:00Z",
    },
    {
      id: 1360,
      actor_name: "proxy-token",
      actor_id: null,
      action: "DELETE",
      target: "/admin/backends/19",
      detail: {},
      at: "2026-08-09T08:00:00Z",
    },
  ],
};

// Usage answers on any group_by, including the row shapes that catch the
// unpriced handling: one fully unpriced, one partly.
const USAGE = [
  {
    key: "batch-etl",
    requests: 900,
    prompt_tokens: 412000,
    completion_tokens: 96000,
    cost_micros: 1204000000,
    unpriced_requests: 0,
  },
  {
    key: "audit@kryton",
    requests: 12,
    prompt_tokens: 3000,
    completion_tokens: 1000,
    cost_micros: 0,
    unpriced_requests: 12,
  },
  {
    key: "mixed",
    requests: 50,
    prompt_tokens: 1000,
    completion_tokens: 500,
    cost_micros: 25000,
    unpriced_requests: 7,
  },
];

function fixtureFor(path) {
  const [base] = path.split("?");
  if (base === "/admin/usage") return USAGE;
  if (base in FIXTURES) return FIXTURES[base];
  return null;
}


export { FIXTURES, USAGE, fixtureFor };
