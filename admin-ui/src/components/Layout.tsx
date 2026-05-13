import { NavLink, Outlet, useNavigate } from 'react-router-dom';
import { apiClient } from '@/lib/api';
import { useAuth } from '@/lib/auth';

export function Layout() {
  const { setOut } = useAuth();
  const nav = useNavigate();

  async function handleLogout() {
    try {
      await apiClient.logout();
    } catch {
      /* ignore */
    }
    setOut();
    nav('/login', { replace: true });
  }

  return (
    <div className="min-h-full flex flex-col">
      <header className="border-b border-zinc-200 dark:border-zinc-800 px-6 py-3 flex items-center justify-between">
        <div className="flex items-center gap-6">
          <h1 className="font-semibold text-lg">CloudCode admin</h1>
          <nav className="flex gap-4 text-sm">
            <Tab to="/" end>
              Dashboard
            </Tab>
            <Tab to="/accounts">Accounts</Tab>
            <Tab to="/agents">Agents</Tab>
            <Tab to="/workspaces">Workspaces</Tab>
            <Tab to="/sessions">Sessions</Tab>
            <Tab to="/audit">Audit</Tab>
          </nav>
        </div>
        <button
          onClick={handleLogout}
          className="text-sm px-3 py-1.5 rounded border border-zinc-300 dark:border-zinc-700 hover:bg-zinc-100 dark:hover:bg-zinc-800"
        >
          Sign out
        </button>
      </header>
      <main className="flex-1 px-6 py-6 max-w-screen-xl w-full mx-auto">
        <Outlet />
      </main>
    </div>
  );
}

function Tab({ to, children, end }: { to: string; children: React.ReactNode; end?: boolean }) {
  return (
    <NavLink
      to={to}
      end={end}
      className={({ isActive }) =>
        `px-2 py-1 rounded ${
          isActive
            ? 'bg-zinc-200 dark:bg-zinc-800 text-zinc-900 dark:text-zinc-100'
            : 'text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100'
        }`
      }
    >
      {children}
    </NavLink>
  );
}
