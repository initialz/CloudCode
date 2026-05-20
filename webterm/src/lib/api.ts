// Tiny fetch wrapper for /api/*. Sends cookies automatically.
// Throws ApiError on non-2xx.

export type ApiError = {
  status: number;
  code: string;
  message: string;
};

const BASE = '/api';

export async function api<T = unknown>(path: string, init: RequestInit = {}): Promise<T> {
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
      code:
        typeof body === 'object' && body && 'error' in body
          ? (body as Record<string, unknown>)['error'] as string
          : 'http_error',
      message:
        typeof body === 'object' && body && 'message' in body
          ? (body as Record<string, unknown>)['message'] as string
          : `HTTP ${res.status}`,
    };
    throw err;
  }
  return body as T;
}

export type MeDto = {
  account: string;
  hub_version?: string;
};

export const apiClient = {
  login: (username: string, token: string) =>
    api<{ ok: true; account: string }>('/login', {
      method: 'POST',
      body: JSON.stringify({ username, token }),
    }),
  logout: () => api<void>('/logout', { method: 'POST' }),
  me: () => api<MeDto>('/me'),
  // Per-user preferences blob (opaque to the hub). `preferences` is
  // `null` if the user has never saved anything; the SPA then falls
  // back to its built-in defaults.
  getPreferences: () =>
    api<{ preferences: unknown }>('/preferences'),
  putPreferences: (prefs: unknown) =>
    api<void>('/preferences', {
      method: 'PUT',
      body: JSON.stringify(prefs),
    }),
};
