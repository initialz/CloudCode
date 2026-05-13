import { Routes, Route } from 'react-router-dom';
import { Layout } from '@/components/Layout';
import { Login } from '@/pages/Login';
import { Dashboard } from '@/pages/Dashboard';
import { Accounts } from '@/pages/Accounts';
import { Audit } from '@/pages/Audit';
import { Stub } from '@/pages/Stub';
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
          <Route path="sessions" element={<Stub title="Sessions" />} />
          <Route path="audit" element={<Audit />} />
        </Route>
      </Routes>
    </AuthProvider>
  );
}
