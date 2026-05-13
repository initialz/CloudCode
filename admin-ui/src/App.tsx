import { Routes, Route } from 'react-router-dom';
import { Layout } from '@/components/Layout';
import { Login } from '@/pages/Login';
import { Dashboard } from '@/pages/Dashboard';
import { Accounts } from '@/pages/Accounts';
import { Agents } from '@/pages/Agents';
import { Audit } from '@/pages/Audit';
import { Sessions } from '@/pages/Sessions';
import { SessionDetail } from '@/pages/SessionDetail';
import { Workspaces } from '@/pages/Workspaces';
import { AuthProvider, RequireAuth } from '@/lib/auth';

export default function App() {
  return (
    <AuthProvider>
      <Routes>
        <Route path="/login" element={<Login />} />
        <Route
          path="/"
          element={
            <RequireAuth>
              <Layout />
            </RequireAuth>
          }
        >
          <Route index element={<Dashboard />} />
          <Route path="accounts" element={<Accounts />} />
          <Route path="agents" element={<Agents />} />
          <Route path="workspaces" element={<Workspaces />} />
          <Route path="sessions" element={<Sessions />} />
          <Route path="sessions/:id" element={<SessionDetail />} />
          <Route path="audit" element={<Audit />} />
        </Route>
      </Routes>
    </AuthProvider>
  );
}
