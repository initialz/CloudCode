// Tiny fetch wrapper for /admin/api/*. Sends cookies automatically;
// throws `ApiError` on non-2xx so callers can pattern-match codes.

export type ApiError = {
  status: number;
  code: string;
  message: string;
};

const BASE = '/admin/api';

export async function api<T = unknown>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const res = await fetch(BASE + path, {
    credentials: 'same-origin',
    headers: {
      'Content-Type': 'application/json',
      ...(init.headers ?? {}),
    },
    ...init,
  });

  if (res.status === 204) {
    return undefined as T;
  }

  const isJson = res.headers.get('content-type')?.includes('application/json');
  const body = isJson ? await res.json() : await res.text();

  if (!res.ok) {
    const err: ApiError = {
      status: res.status,
      code: typeof body === 'object' && body && 'error' in body ? (body as any).error : 'http_error',
      message:
        typeof body === 'object' && body && 'message' in body
          ? (body as any).message
          : `HTTP ${res.status}`,
    };
    throw err;
  }
  return body as T;
}

// Typed wrappers for each endpoint group.

export type AccountDto = {
  name: string;
  token_prefix: string | null;
  created_at: number;
  disabled: boolean;
  /// Agents whitelisted for this account (strict whitelist).
  allowed_agents: string[];
  /// Most recent session.started_at, or null if never used.
  last_used_at: number | null;
  /// At least one session is currently live.
  online: boolean;
  /// Per-account sandbox toggle (replaces agent.toml [sandbox]).
  sandbox_enabled: boolean;
};

export type DashboardDto = {
  accounts: number;
  active_sessions: number;
  sessions_24h: number;
  online_agents: string[];
};

export type SessionDto = {
  session_id: string;
  account: string;
  agent: string;
  workspace: string;
  started_at: number;
  ended_at: number | null;
  ended_reason: string | null;
};

export type AuditEventDto = {
  id: number;
  ts: number;
  kind: string;
  account: string | null;
  agent: string | null;
  session_id: string | null;
  workspace: string | null;
  detail: Record<string, unknown> | null;
};

export type HourlyBucket = { ts: number; count: number };

export const apiClient = {
  login: (token: string) =>
    api<{ ok: true }>('/login', { method: 'POST', body: JSON.stringify({ token }) }),
  logout: () => api<void>('/logout', { method: 'POST' }),
  me: () => api<{ ok: true; hub_version?: string }>('/me'),
  dashboard: () => api<DashboardDto>('/dashboard'),
  sessionsHourly: (hours = 24) =>
    api<HourlyBucket[]>(`/sessions/hourly?hours=${hours}`),
  accounts: {
    list: () => api<AccountDto[]>('/accounts'),
    create: (name: string) =>
      api<{ name: string; token: string }>('/accounts', {
        method: 'POST',
        body: JSON.stringify({ name }),
      }),
    rotate: (name: string) =>
      api<{ name: string; token: string }>(`/accounts/${encodeURIComponent(name)}/rotate`, {
        method: 'POST',
      }),
    toggle: (name: string) =>
      api<void>(`/accounts/${encodeURIComponent(name)}/toggle`, { method: 'POST' }),
    toggleSandbox: (name: string) =>
      api<void>(`/accounts/${encodeURIComponent(name)}/sandbox`, { method: 'POST' }),
    delete: (name: string) =>
      api<void>(`/accounts/${encodeURIComponent(name)}`, { method: 'DELETE' }),
    allowedAgents: (name: string) =>
      api<AllowedAgentsDto>(
        `/accounts/${encodeURIComponent(name)}/allowed-agents`,
      ),
    setAllowedAgents: (name: string, agents: string[]) =>
      api<void>(`/accounts/${encodeURIComponent(name)}/allowed-agents`, {
        method: 'PUT',
        body: JSON.stringify({ agents }),
      }),
  },
  agents: {
    list: () => api<AgentRowDto[]>('/agents'),
    allowedAccounts: (name: string) =>
      api<AllowedAccountsDto>(`/agents/${encodeURIComponent(name)}/allowed-accounts`),
    setAllowedAccounts: (name: string, accounts: string[]) =>
      api<void>(`/agents/${encodeURIComponent(name)}/allowed-accounts`, {
        method: 'PUT',
        body: JSON.stringify({ accounts }),
      }),
    delete: (name: string) =>
      api<void>(`/agents/${encodeURIComponent(name)}`, { method: 'DELETE' }),
    releases: () => api<ReleasesResponseDto>('/agents/releases'),
    update: (name: string, version: string) =>
      api<{ ok: true }>(`/agents/${encodeURIComponent(name)}/update`, {
        method: 'POST',
        body: JSON.stringify({ version }),
      }),
  },
  hub: {
    // Always pulls the latest release; the supervisor re-execs the new
    // binary after a clean exit. Returns 202 before exiting so the
    // frontend can switch to its "waiting for hub to come back" poll.
    update: () =>
      api<{ ok: true; installed: string }>('/hub/update', { method: 'POST' }),
    // Unauthenticated. Used to poll for the hub coming back online
    // after a self-update — /me 401s indefinitely once the in-memory
    // cookie session is wiped during the restart.
    version: () => api<{ version: string }>('/hub-version'),
  },
  workspaces: {
    list: () => api<WorkspaceRowDto[]>('/workspaces'),
  },
  stats: {
    leaderboard: (window: '7d' | '30d', group: 'account' | 'agent') =>
      api<LeaderboardRowDto[]>(`/stats/leaderboard?window=${window}&group=${group}`),
    sessionDuration: (window: '7d' | '30d') =>
      api<SessionDurationDto>(`/stats/session-duration?window=${window}`),
    messagesDaily: (days: number) =>
      api<DailyMessageDto[]>(`/stats/messages-daily?days=${days}`),
    messagesPerSession: (window: '7d' | '30d') =>
      api<MessagesPerSessionDto>(`/stats/messages-per-session?window=${window}`),
    tokensDaily: (days: number) =>
      api<DailyTokenDto[]>(`/stats/tokens-daily?days=${days}`),
  },
  audit: {
    list: (q: Record<string, string | number | undefined>) => {
      const params = new URLSearchParams();
      for (const [k, v] of Object.entries(q)) {
        if (v !== undefined && v !== '') params.set(k, String(v));
      }
      return api<{
        events: AuditEventDto[];
        total: number;
        page: number;
        page_size: number;
      }>(`/audit?${params.toString()}`);
    },
    kinds: () => api<string[]>('/audit/kinds'),
  },
  sessions: {
    list: (q: Record<string, string | number | boolean | undefined>) => {
      const params = new URLSearchParams();
      for (const [k, v] of Object.entries(q)) {
        if (v !== undefined && v !== '' && v !== false) params.set(k, String(v));
      }
      return api<{
        sessions: SessionDto[];
        total: number;
        page: number;
        page_size: number;
      }>(`/sessions?${params.toString()}`);
    },
    detail: (id: string) =>
      api<SessionDetailDto>(`/sessions/${encodeURIComponent(id)}`),
    messages: (id: string, limit = 500) =>
      api<MessageDto[]>(`/sessions/${encodeURIComponent(id)}/messages?limit=${limit}`),
  },
};

export type SessionDetailDto = SessionDto & { message_count: number };

export type MessageDto = {
  id: number;
  ts: number;
  kind: string;
  body: any;
};

export type AllowedAgentsDto = {
  /// Agents currently whitelisted for this account.
  allowed: string[];
  /// Union of historical + online + already-allowed (admin picker pool).
  known: string[];
  /// Subset of `known` that's connected right now.
  online: string[];
};

export type AgentRowDto = {
  name: string;
  online: boolean;
  allowed_account_count: number;
  version: string | null;
  latest_version: string | null;
};

export type ReleaseDto = { tag: string; date: string };
export type ReleasesResponseDto = { releases: ReleaseDto[]; latest: string | null };

export type AllowedAccountsDto = {
  /// Accounts currently whitelisted for this agent.
  allowed: string[];
  /// Every account in the system (the picker pool).
  accounts: string[];
  online: boolean;
};

export type WorkspaceStatus = 'active' | 'saved' | 'fresh';

// ── Stats DTOs ─────────────────────────────────────────────────────────────

export type LeaderboardRowDto = {
  name: string;
  session_count: number;
  total_duration_seconds: number;
  message_count: number;
};

export type DurationBucketDto = {
  label: string;
  from_seconds: number;
  to_seconds: number | null;
  count: number;
};

export type SessionDurationDto = {
  count: number;
  mean_seconds: number;
  median_seconds: number;
  p95_seconds: number;
  max_seconds: number;
  buckets: DurationBucketDto[];
};

export type DailyMessageDto = {
  date: string;
  user: number;
  assistant: number;
  other: number;
};

export type MessageCountBucketDto = {
  label: string;
  from: number;
  to: number | null;
  count: number;
};

export type MessagesPerSessionDto = {
  count: number;
  mean: number;
  median: number;
  p95: number;
  max: number;
  buckets: MessageCountBucketDto[];
};

export type DailyTokenDto = {
  date: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
};

export type WorkspaceRowDto = {
  agent: string;
  account: string;
  workspace: string;
  status: WorkspaceStatus;
  has_client: boolean;
  tmux_alive: boolean;
  agent_online: boolean;
  last_started_at: number | null;
};
