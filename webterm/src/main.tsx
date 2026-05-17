import React from 'react';
import ReactDOM from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import App from './App';
import { apply as applyTheme, getStoredTheme, watchSystem } from './lib/theme';
import './index.css';

// Apply saved theme before React mounts to avoid flash of wrong palette.
applyTheme(getStoredTheme());
watchSystem();

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <BrowserRouter>
      <App />
    </BrowserRouter>
  </React.StrictMode>,
);
