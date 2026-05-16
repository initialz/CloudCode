import { Routes, Route, Navigate } from 'react-router-dom';
import Login from '@/pages/Login';
import Picker from '@/pages/Picker';
import Session from '@/pages/Session';

export default function App() {
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      <Route path="/" element={<Picker />} />
      <Route path="/session" element={<Session />} />
      {/* Catch-all → picker */}
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
