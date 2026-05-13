import { useEffect, useState } from 'react';
import { apiClient, type DashboardDto, type HourlyBucket } from '@/lib/api';
import { StatCard } from '@/components/StatCard';
import { SessionsChart } from '@/components/SessionsChart';

export function Dashboard() {
  const [dash, setDash] = useState<DashboardDto | null>(null);
  const [hourly, setHourly] = useState<HourlyBucket[] | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    Promise.all([apiClient.dashboard(), apiClient.sessionsHourly(24)])
      .then(([d, h]) => {
        if (cancelled) return;
        setDash(d);
        setHourly(h);
      })
      .catch((e: any) => {
        if (cancelled) return;
        setErr(e?.message ?? 'load failed');
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (err) {
    return (
      <div className="rounded border-l-2 border-red-500 bg-red-50 dark:bg-red-950/30 px-3 py-2 text-sm text-red-700 dark:text-red-300">
        {err}
      </div>
    );
  }
  if (!dash || !hourly) {
    return <div className="text-sm text-zinc-500">Loading…</div>;
  }

  const agentSub =
    dash.online_agents.length === 0
      ? <span className="text-zinc-400">(none connected)</span>
      : dash.online_agents.join(', ');

  return (
    <div className="space-y-6">
      <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
        <StatCard label="Accounts" value={dash.accounts} link="/accounts" />
        <StatCard
          label="Active sessions"
          value={dash.active_sessions}
          sub="live now"
          link="/sessions?active=1"
        />
        <StatCard
          label="Sessions (24h)"
          value={dash.sessions_24h}
          link="/sessions"
        />
        <StatCard
          label="Online agents"
          value={dash.online_agents.length}
          sub={agentSub}
        />
      </div>

      <section className="rounded-lg border border-zinc-200 dark:border-zinc-800 p-4 bg-white dark:bg-zinc-900">
        <header className="flex items-baseline justify-between mb-2">
          <h3 className="text-sm font-medium">Sessions started — last 24 hours</h3>
          <span className="text-xs text-zinc-500">UTC</span>
        </header>
        <SessionsChart data={hourly} hours={24} />
      </section>
    </div>
  );
}
