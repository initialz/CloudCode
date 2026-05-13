import { createContext, useContext, useEffect, useState, type ReactNode } from 'react';
import { Navigate, useLocation } from 'react-router-dom';
import { apiClient } from './api';

type AuthState = 'unknown' | 'in' | 'out';

interface AuthCtx {
  status: AuthState;
  setIn: () => void;
  setOut: () => void;
}

const Ctx = createContext<AuthCtx>({
  status: 'unknown',
  setIn: () => {},
  setOut: () => {},
});

export function AuthProvider({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<AuthState>('unknown');

  useEffect(() => {
    apiClient
      .me()
      .then(() => setStatus('in'))
      .catch(() => setStatus('out'));
  }, []);

  return (
    <Ctx.Provider
      value={{
        status,
        setIn: () => setStatus('in'),
        setOut: () => setStatus('out'),
      }}
    >
      {children}
    </Ctx.Provider>
  );
}

export function useAuth() {
  return useContext(Ctx);
}

export function RequireAuth({ children }: { children: ReactNode }) {
  const { status } = useAuth();
  const location = useLocation();

  if (status === 'unknown') {
    return (
      <div className="flex h-full items-center justify-center text-zinc-500">
        Loading…
      </div>
    );
  }
  if (status === 'out') {
    return <Navigate to="/login" replace state={{ from: location }} />;
  }
  return <>{children}</>;
}
