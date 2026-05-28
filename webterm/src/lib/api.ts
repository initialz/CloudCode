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
  real_name?: string | null;
};

export const apiClient = {
  login: (username: string, token: string) =>
    api<{ ok: true; account: string }>('/login', {
      method: 'POST',
      body: JSON.stringify({ username, token }),
    }),
  logout: () => api<void>('/logout', { method: 'POST' }),
  me: () => api<MeDto>('/me'),
  updateMe: (data: { real_name?: string | null }) =>
    api<void>('/me', {
      method: 'PUT',
      body: JSON.stringify(data),
    }),
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

// ── Invite API ───────────────────────────────────────────────────────────────

export type InviteInfo =
  | { valid: true; max_uses: number; used: number; allowed_agents: string[] }
  | { valid: false; reason: string };

export async function getInviteInfo(token: string): Promise<InviteInfo> {
  const res = await fetch(`/api/invite/${encodeURIComponent(token)}/info`);
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

export async function acceptInvite(
  token: string,
  username: string,
  realName?: string | null,
): Promise<{ account: string; token: string }> {
  const res = await fetch(`/api/invite/${encodeURIComponent(token)}/accept`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, real_name: realName || null }),
  });
  const body = await res.json();
  if (!res.ok) {
    throw new Error(body.message || `HTTP ${res.status}`);
  }
  return body;
}

// ── File manager API ─────────────────────────────────────────────────────────

export type FsEntry = {
  name: string;
  kind: 'file' | 'dir' | 'symlink' | 'other';
  size: number;
  mtime_ms: number;
};

export async function listFiles(
  agent: string,
  workspace: string,
  path: string,
  showHidden: boolean,
  signal?: AbortSignal,
): Promise<{ entries: FsEntry[]; error: string | null }> {
  const qs = new URLSearchParams({
    agent,
    workspace,
    path,
    ...(showHidden ? { show_hidden: '1' } : {}),
  });
  const res = await fetch(`/api/files/list?${qs.toString()}`, {
    credentials: 'same-origin',
    signal,
  });
  if (!res.ok) {
    let errMsg = `HTTP ${res.status}`;
    try {
      const body = await res.json() as { error?: string };
      if (body.error) errMsg = body.error;
    } catch {
      // ignore parse error
    }
    throw new Error(errMsg);
  }
  return res.json() as Promise<{ entries: FsEntry[]; error: string | null }>;
}

export function downloadFileUrl(
  agent: string,
  workspace: string,
  path: string,
): string {
  const qs = new URLSearchParams({ agent, workspace, path });
  return `/api/files/download?${qs.toString()}`;
}

export function archiveUrl(
  agent: string,
  workspace: string,
  paths: string[],
): string {
  const qs = new URLSearchParams({
    agent,
    workspace,
    paths: paths.join(','),
  });
  return `/api/files/archive?${qs.toString()}`;
}

// ── File delete API ─────────────────────────────────────────────────────────

export async function deleteFiles(
  agent: string,
  workspace: string,
  paths: string[],
): Promise<{ deleted: string[]; error: string | null }> {
  const qs = new URLSearchParams({
    agent,
    workspace,
    paths: paths.join(','),
  });
  const res = await fetch(`/api/files/delete?${qs.toString()}`, {
    method: 'DELETE',
    credentials: 'same-origin',
  });
  if (!res.ok) {
    let errMsg = `HTTP ${res.status}`;
    try {
      const body = await res.json() as { error?: string };
      if (body.error) errMsg = body.error;
    } catch { /* ignore */ }
    throw new Error(errMsg);
  }
  return res.json();
}

// ── File upload API ──────────────────────────────────────────────────────────

export type UploadResult = {
  name: string;
  bytes_written: number;
  error: string | null;
};

export type UploadItem = { file: File; relativePath: string };

export function uploadFiles(
  agent: string,
  workspace: string,
  path: string,
  files: UploadItem[],
  onProgress?: (loaded: number, total: number) => void,
): { promise: Promise<UploadResult[]>; abort: () => void } {
  const xhr = new XMLHttpRequest();
  const promise = new Promise<UploadResult[]>((resolve, reject) => {
    const formData = new FormData();
    for (const item of files) {
      // Third arg overrides the filename in multipart Content-Disposition,
      // so the server sees the relative path (e.g. "src/main.rs").
      formData.append('file', item.file, item.relativePath);
    }
    const qs = new URLSearchParams({ agent, workspace, path });
    xhr.open('POST', `/api/files/upload?${qs.toString()}`);
    xhr.withCredentials = true;
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable && onProgress) {
        onProgress(e.loaded, e.total);
      }
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          const body = JSON.parse(xhr.responseText);
          resolve(body.results ?? []);
        } catch {
          reject(new Error('Invalid response'));
        }
      } else {
        reject(new Error(`Upload failed: HTTP ${xhr.status}`));
      }
    };
    xhr.onerror = () => reject(new Error('Upload failed: network error'));
    xhr.onabort = () => reject(new Error('Upload aborted'));
    xhr.send(formData);
  });
  return { promise, abort: () => xhr.abort() };
}
