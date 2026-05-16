// xterm.js wrapper component.
// Exposes a ref interface so the parent (Session) can call write/fit/resize.

import { useEffect, useRef, forwardRef, useImperativeHandle } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import '@xterm/xterm/css/xterm.css';
import { effectiveTheme, getStoredTheme } from '@/lib/theme';

export type TermHandle = {
  write: (data: Uint8Array | string) => void;
  fit: () => { cols: number; rows: number } | null;
  focus: () => void;
  setDark: (dark: boolean) => void;
};

type Props = {
  onData: (data: string) => void;
};

function darkTheme() {
  return { background: '#18181b', foreground: '#fafafa', cursor: '#fafafa' };
}

function lightTheme() {
  return { background: '#ffffff', foreground: '#18181b', cursor: '#18181b' };
}

const Term = forwardRef<TermHandle, Props>(({ onData }, ref) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);

  useImperativeHandle(ref, () => ({
    write(data: Uint8Array | string) {
      termRef.current?.write(data);
    },
    fit() {
      if (!fitRef.current || !termRef.current) return null;
      fitRef.current.fit();
      const { cols, rows } = termRef.current;
      return { cols, rows };
    },
    focus() {
      termRef.current?.focus();
    },
    setDark(dark: boolean) {
      if (termRef.current) {
        termRef.current.options.theme = dark ? darkTheme() : lightTheme();
      }
    },
  }));

  useEffect(() => {
    if (!containerRef.current) return;

    const isDark = effectiveTheme(getStoredTheme()) === 'dark';
    const term = new Terminal({
      cursorBlink: true,
      scrollback: 10000,
      fontFamily: 'ui-monospace, Menlo, Monaco, monospace',
      fontSize: 14,
      theme: isDark ? darkTheme() : lightTheme(),
    });

    const fit = new FitAddon();
    const links = new WebLinksAddon();

    term.loadAddon(fit);
    term.loadAddon(links);
    term.open(containerRef.current);
    fit.fit();

    termRef.current = term;
    fitRef.current = fit;

    const disposable = term.onData(onData);

    return () => {
      disposable.dispose();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
    // onData identity is stable (parent uses useCallback)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return <div ref={containerRef} className="xterm-host" />;
});

Term.displayName = 'Term';
export default Term;
