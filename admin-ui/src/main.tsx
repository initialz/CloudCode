import React from 'react';
import ReactDOM from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import App from './App';
import { apply as applyTheme, getStoredTheme, watchSystem } from './lib/theme';
import './index.css';

// Apply the saved theme before React renders so the first paint
// is already in the right palette (no flash of wrong-mode chrome).
applyTheme(getStoredTheme());
watchSystem();

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <BrowserRouter basename="/admin">
      <App />
    </BrowserRouter>
  </React.StrictMode>,
);
