import { Routes, Route } from 'react-router-dom';
import { Layout } from '@/components/Layout';
import { Login } from '@/pages/Login';
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
          <Route index element={<Stub title="Dashboard" />} />
          <Route path="accounts" element={<Stub title="Accounts" />} />
          <Route path="sessions" element={<Stub title="Sessions" />} />
          <Route path="audit" element={<Stub title="Audit" />} />
        </Route>
      </Routes>
    </AuthProvider>
  );
}
