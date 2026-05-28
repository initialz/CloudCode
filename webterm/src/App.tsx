import { Routes, Route, Navigate } from 'react-router-dom';
import Invite from '@/pages/Invite';
import Login from '@/pages/Login';
import Workbench from '@/pages/Workbench';

export default function App() {
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      <Route path="/invite/:token" element={<Invite />} />
      <Route path="/" element={<Workbench />} />
      {/* Catch-all → workbench */}
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
